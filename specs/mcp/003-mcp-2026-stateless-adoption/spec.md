---
aliases:
  - MCP 2026-07-28 stateless spec tracking
tags:
  - sdd
  - spec
  - research
  - mcp
created: 2026-08-05
status: draft
related:
  - "[[MOC-specs]]"
---

# Feature: Track MCP Specification 2026-07-28 (Stateless Revision) for Future mcpls Compatibility

> [!info] Metadata
> **Author**: k05h31
> **Branch**: N/A (research finding, no branch yet)
> **Type**: research
> **Priority**: P3

## 1. Overview

### Problem Statement

The Model Context Protocol specification advanced from `2025-11-25` — the
version mcpls's own project rules cite as the "authoritative protocol
reference" in [`.claude/rules/continuous-improvement.md`](../../../.claude/rules/continuous-improvement.md)
— to `2026-07-28`, described by the MCP maintainers as the largest protocol
revision since launch. The revision removes the `initialize` /
`notifications/initialized` handshake entirely and makes MCP fully stateless:
every request must instead carry protocol version and client capabilities via
`_meta` fields.

mcpls's MCP server surface (`crates/mcpls-core/src/mcp/`, built on the `rmcp`
crate, currently pinned to `rmcp = "3.0.0"` in the workspace
`Cargo.toml`) implements `get_info()` / `ServerCapabilities` on top of rmcp's
current stateful `initialize` model. If the removal of `initialize` becomes a
hard protocol requirement, mcpls's MCP-facing capability negotiation surface
will need a migration path.

> [!warning] Two distinct handshakes — do not conflate them
> mcpls has **two independent `initialize` handshakes** and this finding
> concerns only one of them:
> 1. **MCP-side** (client ↔ mcpls, via `rmcp`, `crates/mcpls-core/src/mcp/server.rs`) —
>    this is the handshake the 2026-07-28 spec removes. In scope here.
> 2. **LSP-side** (mcpls ↔ language server, `crates/mcpls-core/src/lsp/lifecycle.rs`) —
>    this is the handshake recently touched by commit `bc95b89` (`position_encodings`
>    negotiation, PR #289). It is unrelated to the MCP spec revision and speaks
>    LSP 3.17, not MCP. It is **out of scope** for this finding but mentioned
>    because the original research prompt referenced it; no LSP-side change is
>    implied or required by MCP 2026-07-28.

The upstream Rust SDK (`rmcp`) released v3.1.0 on 2026-07-31 with only
*initial* conformance work toward 2026-07-28 (strict stateless metadata
validation, SEP-2260 stream-based request association, honoring
`supported_protocol_versions` during negotiation) — explicitly framed as a
phased, Tier-based rollout, not a finished implementation. mcpls therefore
cannot adopt the new model yet; this spec exists to **track** the change and
define what mcpls will need to do once `rmcp` reaches conformance, not to
implement it now.

### Goal

mcpls has a documented, discoverable plan for migrating its MCP-side
capability negotiation away from the `initialize`/`notifications/initialized`
handshake once `rmcp` ships full 2026-07-28 conformance — so the transition,
when it happens, is a scoped implementation task rather than a surprise
breaking change discovered at an `rmcp` major-version bump.

### Out of Scope

- Implementing any stateless-protocol code changes now — `rmcp` conformance
  is incomplete (Tier 1 in progress as of v3.1.0)
- Changing the LSP-side `initialize` handshake (`lsp/lifecycle.rs`,
  `bc95b89`/#289) — that speaks LSP 3.17 and is unaffected by this MCP spec
  revision
- Implementing Streamable HTTP transport changes (`Mcp-Session-Id` removal) —
  mcpls's primary transport is stdio; HTTP transport itself is tracked
  separately in issue #122
- Implementing the redesigned Tasks extension — tracked separately in issue #119
- Implementing `subscriptions/listen` replacing `resources/subscribe` /
  `resources/unsubscribe` — resource subscriptions were added in
  [[mcp/002-mcp-resources-diagnostics/spec|Spec mcp/002]] under the 2025-11-25 model;
  re-scoping them is a future spec once the MRTR/subscriptions API stabilizes
  upstream in `rmcp`
- Updating `.claude/rules/continuous-improvement.md`'s reference spec version —
  premature until `rmcp` and mcpls actually target 2026-07-28

## 2. User Stories

### US-001: Maintainer plans ahead of a breaking rmcp upgrade

AS A mcpls maintainer
I WANT a written record of what changes in MCP 2026-07-28 and which parts of
`crates/mcpls-core/src/mcp/` they touch
SO THAT when `rmcp` ships full conformance (a likely future breaking semver
bump), I can scope the migration instead of reverse-engineering the diff
between spec versions under time pressure.

**Acceptance criteria:**
```
GIVEN this spec exists in .local/specs/mcp/003-mcp-2026-stateless-adoption/
WHEN a maintainer next reviews rmcp's release notes for a version that claims
     2026-07-28 conformance
THEN this spec's Functional Requirements table gives them a checklist of the
     mcpls-side surfaces that need review before upgrading
```

### US-002: mcpls avoids being surprised by initialize removal

AS A mcpls maintainer
I WANT the MCP-side `initialize` handshake dependency in
`crates/mcpls-core/src/mcp/server.rs` explicitly flagged as at-risk
SO THAT a future `rmcp` major bump that drops stateful `initialize` doesn't
silently break mcpls's capability negotiation or client compatibility.

**Acceptance criteria:**
```
GIVEN mcpls-core/src/mcp/server.rs implements get_info() / ServerCapabilities
      against rmcp's current initialize-based API
WHEN rmcp releases a version implementing the 2026-07-28 stateless model as
     the default/only mode
THEN mcpls has a pre-identified list (FR-001..FR-004 below) of what must
     change, rather than starting discovery from zero
```

## 3. Functional Requirements

These are tracking/planning requirements, not implementation directives —
this finding is P3 research, and `rmcp` conformance work is incomplete.

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN `rmcp` publishes a release claiming 2026-07-28 conformance THE SYSTEM'S maintainers SHALL re-review `crates/mcpls-core/src/mcp/server.rs`'s `get_info()`/`ServerCapabilities` implementation for compatibility with per-request `_meta` protocol-version/capability fields replacing `initialize` | must |
| FR-002 | mcpls's MCP-side capability negotiation SHALL have a documented migration path away from the `initialize`/`notifications/initialized` handshake before upgrading to an `rmcp` version that removes stateful `initialize` as a supported mode | must |
| FR-003 | IF `rmcp` retains a backward-compatible/dual-mode `initialize` path during its deprecation window (Roots/Sampling/Logging carry a 12-month deprecation per the spec) THEN mcpls SHOULD defer the stateless migration until that window's expiry is imminent, to avoid churn on an unstable upstream API | should |
| FR-004 | WHEN mcpls does migrate, THE SYSTEM SHALL re-audit `notifications.rs`'s push-based diagnostics caching against the `subscriptions/listen` replacement for `resources/subscribe`/`resources/unsubscribe`, since [[mcp/002-mcp-resources-diagnostics/spec|Spec mcp/002]]'s design assumes the 2025-11-25 subscribe/unsubscribe API | should |
| FR-005 | THE SYSTEM'S issue tracker SHALL retain a link between this spec, issue #119 (tasks extension redesign), and issue #122 (Streamable HTTP parity), since all three are touched by different facets of the same MCP spec revision | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Backward compatibility | mcpls SHALL NOT upgrade its `rmcp` dependency to a version that makes the stateless (no-`initialize`) model the *only* supported mode until `rmcp`'s own Tier rollout (per its roadmap) reaches a stage the maintainers judge stable — avoids adopting an unstable protocol surface mid-rollout |
| NFR-002 | Compatibility window | Given Roots, Sampling, and Logging carry a 12-month deprecation window in 2026-07-28, mcpls's migration timeline SHOULD align with that window rather than rushing ahead of it, since mcpls does not currently implement Sampling or Elicitation (Roots/Logging usage should be confirmed — `[NEEDS CLARIFICATION: does mcpls currently use MCP Roots or Logging features from crates/mcpls-core/src/mcp/, and if so, are they exposed to clients today?]`) |
| NFR-003 | Traceability | This spec SHALL be discoverable from `.local/specs/MOC-specs.md` and cross-linked with issues #119 and #122 so a future continuous-improvement cycle surfaces it automatically when scanning open research items |
| NFR-004 | No premature action | No source code under `crates/` SHALL be modified as a result of this spec — it is a tracking/planning artifact only, consistent with its P3/research classification |

## 5. Data Model

Not applicable — this is a protocol-compatibility tracking spec with no new
mcpls data entities. The relevant "entities" are external protocol constructs
that mcpls's MCP layer will eventually need to represent:

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| Request `_meta` protocol fields | Replaces `initialize`-negotiated state; carried per-request in 2026-07-28 | `io.modelcontextprotocol/protocolVersion`, `io.modelcontextprotocol/clientCapabilities` |
| `server/discover` response | New mandatory RPC servers must answer, advertising identity/capabilities without a prior handshake | supported protocol versions, capabilities, server identity |
| `InputRequiredResult` | Replaces server-initiated `roots/list`/`sampling/createMessage`/`elicitation/create` requests under the MRTR pattern | `resultType`, retry payload |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| `rmcp` ships a version that supports 2026-07-28 as opt-in alongside legacy `initialize` | mcpls should stay on the legacy path until this spec is revisited and superseded by an implementation spec |
| `rmcp` ships a version that drops legacy `initialize` support entirely (breaking) | mcpls's `Cargo.toml` `rmcp` version pin acts as the safety net — do not bump past that version until the migration described here is actually implemented |
| A future MCP client sends 2026-07-28-style per-request `_meta` fields to a pre-migration mcpls server | Current `rmcp = "3.0.0"`-based server does not understand these fields; behavior depends on `rmcp`'s own backward-compatibility handling, not mcpls code — `[NEEDS CLARIFICATION: does rmcp 3.0.0 reject or silently ignore unrecognized _meta fields from newer clients?]` |
| Position-encoding negotiation (`bc95b89`, LSP-side) is mistaken for MCP-side handshake work in a future PR | Reviewers should point to this spec's Overview callout distinguishing the two handshakes |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Spec discoverability | Spec is linked from `.local/specs/MOC-specs.md` and readable without additional context |
| SC-002 | Issue cross-linking | Issues #119 and #122 (or their successors) reference this spec, or this spec is referenced from a future issue tracking the actual migration |
| SC-003 | No premature implementation | Zero commits to `crates/` reference this spec as their justification until `rmcp` conformance is confirmed by a maintainer |

## 8. Agent Boundaries

### Always (without asking)
- Treat this spec as read-only tracking documentation
- Re-check `rmcp`'s changelog against FR-001 whenever a dependency-update
  cycle touches `rmcp`

### Ask First
- Opening an implementation spec/PR based on this tracking spec
- Bumping the `rmcp` version past one that changes `initialize` semantics

### Never
- Modify `crates/mcpls-core/src/mcp/` or `crates/mcpls-core/src/lsp/` source
  code as a direct result of this spec
- Update `.claude/rules/continuous-improvement.md`'s cited spec version
  without a maintainer decision — that reference documents current behavior,
  not aspirational future behavior

## 9. Open Questions

- [NEEDS CLARIFICATION: What is mcpls's actual timeline trigger for revisiting this — a specific `rmcp` version, a calendar date, or "when issue #119/#122 work resumes"?]
- [NEEDS CLARIFICATION: Does mcpls currently use MCP Roots or Logging features (`crates/mcpls-core/src/mcp/`), which are formally deprecated in 2026-07-28 with a 12-month window?]
- [NEEDS CLARIFICATION: Does `rmcp` 3.0.0 (mcpls's current pin) reject or silently ignore unrecognized 2026-07-28-style `_meta` fields sent by a forward-compatible client?]
- [NEEDS CLARIFICATION: Should this spec be re-filed as a GitHub issue (P3, `research` label) per the project's issue-filing protocol, or does the spec artifact alone satisfy tracking for now?]

## 10. See Also

- [[MOC-specs]] — all specifications
- [MCP Specification 2026-07-28 changelog](https://modelcontextprotocol.io/specification/2026-07-28/changelog)
- [MCP 2026-07-28 release candidate announcement](https://blog.modelcontextprotocol.io/posts/2026-07-28-release-candidate/)
- [rmcp v3.1.0 release notes](https://github.com/modelcontextprotocol/rust-sdk/releases/tag/rmcp-v3.1.0)
- [MCP Specification 2025-11-25](https://modelcontextprotocol.io/specification/2025-11-25/) — mcpls's current cited reference in `.claude/rules/continuous-improvement.md`, superseded by 2026-07-28 above
- [GitHub issue bug-ops/mcpls#119](https://github.com/bug-ops/mcpls/issues/119) — "support MCP 2025-11-25 tasks/call" (P4), directly affected by the Tasks extension redesign in 2026-07-28
- [GitHub issue bug-ops/mcpls#122](https://github.com/bug-ops/mcpls/issues/122) — competitive-parity playbook / Streamable HTTP transport, affected by session-header removal in 2026-07-28
- Commit `bc95b89` — LSP-side `position_encodings` handshake fix (PR #289); referenced only to clarify it is a *different* handshake from the one this spec tracks
