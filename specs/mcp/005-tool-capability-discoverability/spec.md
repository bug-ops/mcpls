---
aliases:
  - Tool capability discoverability
  - tools/list capability-aware pruning
tags:
  - sdd
  - spec
  - research
  - mcp
  - discoverability
created: 2026-09-21
status: draft
related:
  - "[[constitution]]"
  - "[[mcp/001-mcp-tool-surface-and-routing/spec|mcp-tool-surface-and-routing]]"
---

# Feature: Tool Capability Discoverability in `tools/list` for Multi-Server Sessions

> [!info] Metadata
> **Type**: research (parity/UX gap)
> **Priority**: P4
> **Related issues**: #461

## 1. Overview

### Problem Statement

mcpls's MCP `tools/list` response is generated once from a static, compile-time table of all
20 `#[tool]`-annotated handlers (`declared_tool_router()` in
`crates/mcpls-core/src/mcp/server.rs:363`, built via `build_tool_router` at `:423`). The list is
identical regardless of which LSP servers are actually configured for the session or what
capabilities those servers advertise in their `initialize` response.

An AI agent connecting to mcpls therefore has no way to learn, up front, that a given tool will
be refused for a given language. For example, `call_hierarchy_prepare` will be rejected with a
capability error for any LSP server that doesn't advertise `callHierarchyProvider` — the agent
can only discover this by attempting the call and receiving the error back.

The reference project `isaacphi/mcp-language-server` (Go, ~1595 stars, single-LSP-server
architecture) solves this for its simpler case: it only registers a tool in `tools/list` when the
one connected LSP server actually advertises the matching capability, so its `tools/list`
response accurately reflects what the server can do
([isaacphi/mcp-language-server](https://github.com/isaacphi/mcp-language-server)).

**Why this is not a simple port of that approach.** mcpls is architecturally a *multi-server*
bridge: it can run several LSP servers concurrently for different languages/file types within a
single session (per `.claude/rules/continuous-improvement.md`, Critical Paths: "Multiple LSP
servers run concurrently; one failing doesn't affect others"). MCP's `tools/list` is a single
global list for the whole session — there is no per-document or per-language scoping in the
protocol. For mcpls specifically:

- Pruning `tools/list` to the **intersection** of all connected servers' capabilities would hide
  tools that genuinely work for some configured languages (e.g. hide `call_hierarchy_prepare`
  entirely because one of five configured servers lacks `callHierarchyProvider`, even though the
  other four support it).
- Pruning to the **union** would keep advertising tools that fail for whichever specific language
  lacks the capability — this reintroduces the exact discoverability gap this finding describes,
  just scoped to a subset of languages instead of all of them.
- A real fix most likely needs either per-tool capability metadata surfaced through tool
  descriptions/annotations (so an agent can read, per configured server, which tools are
  supported), or a lightweight meta-tool the agent can query for per-server/per-language
  capability support — not a blanket list-time filter, which is the shape that works for a
  single-server bridge but actively misleads in a multi-server one.

Note this is a **discoverability/UX gap, not a correctness bug**. mcpls's per-call capability
gating (`Translator::require_capability`,
`crates/mcpls-core/src/bridge/translator/routing.rs:568`) already returns a well-typed,
unambiguous capability error when a tool is called against a server that lacks it — this path was
recently hardened by the typed capability-gating refactor in #412 and #436. There is no risk of a
silent empty or wrong result; the gap is purely that the agent cannot know this *before* calling.

### Goal

An AI agent connected to mcpls in a multi-server session can determine, without first making a
failing `tools/call`, whether a given tool is meaningfully usable for the language/server it
currently cares about.

### Out of Scope

- Choosing and implementing the exact discovery mechanism (tool annotations vs. a meta-tool vs.
  enriched `description` text) — left open, see FR-001 and Open Questions.
- Changing or weakening `Translator::require_capability` / the existing per-call capability error
  behavior in `crates/mcpls-core/src/bridge/translator/routing.rs:568` — that path is already
  correct and is explicitly preserved (see NFR-002).
- Any change to how LSP servers are discovered, spawned, or matched to file types
  (`crates/mcpls-core/src/config/`) — this spec only concerns what the MCP layer *communicates*
  about capabilities already known to the bridge.
- A `/sdd plan` technical design — per this project's research-spec threshold, this finding stops
  at `specify`; a plan phase is deferred until a mechanism is chosen (see Open Questions).

## 2. User Stories

### US-001: AI agent avoids trial-and-error tool discovery in a multi-language session

AS AN AI coding agent connected to mcpls with several LSP servers configured (e.g. rust-analyzer
for Rust files, pyright for Python files)
I WANT to know, before calling a tool, whether it is supported for the language/server I'm
currently working with
SO THAT I don't waste a round-trip on a call that is guaranteed to fail with a capability error,
and so I can choose an alternative approach proactively.

**Acceptance criteria:**
```
GIVEN a session with rust-analyzer (supports callHierarchyProvider) and a second LSP server that
  does not advertise callHierarchyProvider
WHEN the agent inspects available tool information for the second server's language
THEN the agent can determine that call_hierarchy_prepare is not supported for that language
  without first issuing a tools/call that returns a capability error
```

### US-002: mcpls operator configuring multiple language servers gets accurate advertised capabilities

AS A mcpls operator running a multi-language workspace (e.g. Rust + TypeScript + Python via three
different LSP servers)
I WANT the MCP tool surface to reflect, in some discoverable way, which of my configured servers
support which tools
SO THAT I can trust that "tool exists" information from mcpls without needing to separately read
each LSP server's own capability documentation.

**Acceptance criteria:**
```
GIVEN three LSP servers configured for three different languages, with differing capability sets
WHEN the operator or their AI agent queries mcpls for tool availability
THEN the response distinguishes "not supported anywhere in this session" from "supported for some
  configured languages but not others" from "supported for all configured languages"
```

## 3. Functional Requirements

Use EARS notation. Prefix with FR-NNN. The exact mechanism is intentionally left open — see the
marked clarification below — because this is a research/parity spec, not an implementation plan.

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN an AI agent needs to determine whether a tool is usable for a given configured LSP server/language THE SYSTEM SHALL provide a way to answer this without a failing `tools/call` [NEEDS CLARIFICATION: exact discovery mechanism — tool annotations on the existing `tools/list` entries vs. a dedicated meta-tool (e.g. `get_server_capabilities`) vs. enriched per-tool `description` text listing supporting servers] | must |
| FR-002 | WHEN a tool is not supported by *any* currently configured LSP server THE SYSTEM SHALL surface that distinctly from "supported by some but not all configured servers" (per US-002's three-way distinction) | should |
| FR-003 | WHEN LSP servers are added, removed, or respawned during a session (see `lsp/001-lsp-server-lifecycle-and-respawn`) THE SYSTEM SHALL keep any exposed capability-discovery information consistent with the currently connected servers' actual `initialize` responses | should |
| FR-004 | IF a chosen mechanism changes the shape or contents of the standard MCP `tools/list` response THEN THE SYSTEM SHALL verify the change stays within valid MCP protocol schema (tool `name`, `description`, `inputSchema`, and optional fields only) — no invention of non-standard top-level fields the MCP spec does not define | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Protocol compatibility | `tools/list` SHALL remain a valid, spec-conformant MCP response at all times — any capability-awareness addition must be expressible within the existing `Tool` schema (e.g. `description` text) or as an additional standard tool, never as a breaking change to response shape |
| NFR-002 | Behavioral non-regression | The existing per-call capability gating in `Translator::require_capability` (`crates/mcpls-core/src/bridge/translator/routing.rs:568`) SHALL NOT be weakened, removed, or bypassed by whatever discovery mechanism is chosen — it remains the authoritative enforcement point regardless of what `tools/list` advertises |
| NFR-003 | Multi-server correctness | Whatever mechanism is chosen SHALL NOT collapse per-server capability differences into a single global true/false per tool (the intersection/union failure modes described in the Problem Statement) — it must be able to represent "supported for server A, not server B" |
| NFR-004 | No silent staleness | If capability-discovery information is cached or computed once at startup, it SHALL be invalidated or recomputed on LSP server respawn (see FR-003) rather than silently going stale |

## 5. Data Model

No new persistent data entities are introduced by this spec. The relevant existing data is:

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| LSP `ServerCapabilities` | Already received and held per-server after `initialize`, per LSP 3.17 | `callHierarchyProvider`, `renameProvider`, `definitionProvider`, etc. (booleans or options) |
| MCP `Tool` | Standard MCP tool descriptor returned in `tools/list` | `name`, `description`, `inputSchema` — any FR-001 mechanism must fit within or extend this shape in a spec-conformant way |

A future plan phase will need to define how per-server `ServerCapabilities` (already tracked
somewhere in the LSP client layer) gets correlated with the static tool table to produce whatever
FR-001's chosen output is.

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| No LSP servers configured yet (empty session) | Capability-discovery information SHOULD indicate "no servers connected" rather than silently reporting all tools as unsupported or supported |
| A configured LSP server has not finished `initialize` yet | Capability-discovery information for that server SHOULD reflect "pending/unknown," not be conflated with "unsupported" |
| A single LSP server supports a capability but is respawning after a crash (see `lsp/001`) | Capability-discovery information SHOULD reflect the server's last-known or in-flight state without crashing `tools/list`/the discovery mechanism itself |
| All configured servers support a tool | Discovery mechanism SHOULD indicate universal support without extra ceremony (avoid noisy output for the common case) |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | This spec is re-evaluated once a mechanism is chosen and a `/sdd plan` is produced | Before any implementation PR touching `crates/mcpls-core/src/mcp/server.rs`'s tool router for this purpose |
| SC-002 | Any implementation of FR-001 passes NFR-001 through NFR-004 | 100% — verified in the future plan's testing strategy |

## 8. Agent Boundaries

### Always (without asking)
- Treat this spec as read-only research context; do not modify `crates/mcpls-core/src/mcp/server.rs` or `crates/mcpls-core/src/bridge/translator/routing.rs` as a side effect of filing or refining this spec.

### Ask First
- Choosing the FR-001 discovery mechanism and promoting this spec into an implementation-ready plan.
- Filing a GitHub issue for this finding, if one does not already exist.

### Never
- Weaken or bypass `Translator::require_capability` (`crates/mcpls-core/src/bridge/translator/routing.rs:568`) as a side effect of implementing any FR-001 mechanism (NFR-002).
- Collapse per-server capability state into a single global boolean per tool (NFR-003).

## 9. Open Questions

- [NEEDS CLARIFICATION: exact discovery mechanism — tool annotations on `tools/list` entries vs. a dedicated meta-tool vs. enriched per-tool `description` text listing which configured servers support it]
- [NEEDS CLARIFICATION: should capability-discovery information be computed lazily per query, or maintained incrementally as servers connect/respawn — ties into NFR-004 and FR-003]
- [NEEDS CLARIFICATION: is there operator/agent demand for this at all, or does the existing well-typed per-call capability error (already hardened by #412/#436) suffice in practice — tracked as #461, still open as of 2026-09-21]

## 10. See Also

- [[constitution]] — project principles
- [[MOC-specs]] — all specifications
- [[mcp/001-mcp-tool-surface-and-routing/spec|mcp-tool-surface-and-routing]] — the static tool
  router (`declared_tool_router`, `ToolRouter`/`ToolKind`/`ServerId`) that FR-001 would need to
  extend or wrap
- [isaacphi/mcp-language-server](https://github.com/isaacphi/mcp-language-server) — reference
  project whose single-LSP-server `tools/list` capability-gated registration motivated this
  finding; see Problem Statement for why mcpls's multi-server architecture makes this non-trivial
  to port directly
- #412, #436 — already-merged typed capability-gating refactor for
  `Translator::require_capability`; establishes that the per-call enforcement path this spec must
  not weaken (NFR-002) is already solid
