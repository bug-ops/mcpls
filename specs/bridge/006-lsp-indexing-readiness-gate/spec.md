---
aliases:
  - LSP Indexing Readiness Gate
  - Indexing Readiness Gate
tags:
  - sdd
  - spec
  - bridge
  - lsp
  - reliability
created: 2026-09-20
status: implemented
related:
  - "[[constitution]]"
---

# Feature: LSP Indexing Readiness Gate

> [!info] Metadata
> **Author**: Andrei G. (k05h31@gmail.com)
> **Branch**: fix/lsp-indexing-readiness-gate
> **Source**: continuous-improvement research cycle finding, P1, bug

> [!success] Resolution
> Implemented by commits `dada6c5` and `6c808df` (PR #421), closing #420. Adds an `IndexingState`
> (`Unknown`/`Loading`/`Ready`) tracked per server in `NotificationCache`, fed by rust-analyzer's
> `experimental/serverStatus` notification (also newly declared as a client capability during the
> initialize handshake, resolving the root cause that no readiness signal could ever arrive).
> `Translator::wait_for_indexing_ready` polls (bounded, 30s default — resolving NFR-001's open
> question) while `Loading` and returns a new `Error::WorkspaceIndexing` instead of proceeding;
> servers that never report indexing pay no added latency (FR-007). The gating decision is
> centralized in `prepare_gated_document` via a new `IndexingGate::{Required,NotRequired}`
> parameter declared explicitly at every call site, rather than a silently-missing default.
> `get_document_symbols` and `workspace_symbol_search` remain ungated, resolving FR-008's open
> question in favor of the zeenix precedent (file-local analysis is valid mid-index) for the
> former; the latter's gating was deferred as a separate open question. A respawned server's
> tracked `IndexingState` resets to `Unknown`. Generic `$/progress`-based indexing detection for
> non-rust-analyzer servers (this spec's FR-006/NFR-003 server-agnostic requirement) was
> deliberately deferred out of this fix and tracked separately in #422, along with call-hierarchy
> and workspace-symbol gating.

## 1. Overview

### Problem Statement

mcpls's production read-tool handlers (`get_hover`, `get_definition`, `get_references`,
`get_completions`, `get_code_actions` — implemented in
`crates/mcpls-core/src/bridge/translator/navigation.rs`, `assist.rs`, `edits.rs`) gate
each request only on whether the routed LSP server *advertises* the relevant capability,
via `Translator::prepare_gated_document` (e.g. checking `hoverProvider` is present in
`ServerCapabilities`). They do not wait for the server to finish its initial workspace
indexing before issuing the request.

For servers with an async workspace-indexing phase after the `initialize`/`initialized`
handshake — rust-analyzer is the primary example, but pyright, gopls, and tsserver all
have comparable async project-load phases — capabilities are advertised immediately at
`initialize` response time, well before indexing completes. During that window (up to
tens of seconds on a large workspace), these read tools receive `null`/`[]` responses
from the LSP server: the same response shape returned for "this position genuinely has
no hover info" / "this symbol genuinely has zero references" / "there is genuinely no
definition here". The MCP tool caller (an AI coding agent) cannot distinguish
"index not ready yet" from "this really is unused/undefined", and the wrong
interpretation is the one that tends to get acted on — e.g. an agent concluding a
function is dead code and deleting it, when references simply had not been indexed yet.

`ServerState::is_ready()` (`crates/mcpls-core/src/lsp/lifecycle.rs:125`) only tracks
completion of the `initialize` → `initialized` LSP handshake, not workspace-load/indexing
completion. No workspace-indexing-completion signal exists anywhere in the production
path under `crates/mcpls-core/src/` — confirmed via source inspection.

mcpls *does* have working indexing-readiness-detection logic already, but it is confined
to test-only helpers (`wait_for_indexing_ready`, hover-probe polling) in
`crates/mcpls-core/tests/ra_e2e.rs`,
`crates/mcpls-core/tests/integration/rust_analyzer_tests.rs`, and
`crates/mcpls-core/tests/e2e/protocol_tests.rs` — built to stabilize integration tests
(see closed issues #121, #127) and never promoted into the production request path.

A comparable, actively-maintained reference project in the same problem space,
[`zeenix/rust-analyzer-mcp`](https://github.com/zeenix/rust-analyzer-mcp), just fixed
exactly this bug in its own production handlers (commit `a772c827`, "wait for the index
before answering about a symbol"). Their fix gates `hover`/`definition`/`references`/
`completion`/`code_actions` on an index-loaded signal (fed by rust-analyzer's
`experimental/serverStatus` notification) with a bounded timeout, and returns a clear,
loud error ("rust-analyzer is still loading the workspace after Ns...") instead of
answering from a partial index. They deliberately leave `document_symbols` ungated,
reasoning single-file symbol extraction is correct even mid-workspace-load — the same
asymmetry likely applies to mcpls's `get_document_symbols`.

mcpls has direct precedent for treating "silent incomplete/misleading result" bugs as
P1: issue #244 ("get_diagnostics tool silently omits cargo-check/flycheck warnings for
rust-analyzer") was filed and fixed as P1 for the same class of problem — a response
that looks complete/normal but is actually silently missing data due to LSP-server
timing.

### Goal

Once this feature ships, read tools whose result depends on whole-workspace analysis
(hover, definition, references, completions, code actions) will never hand an AI agent
an unqualified empty/null result while the routed LSP server is still completing its
initial workspace load — the agent either receives a result computed against a
sufficiently-indexed workspace, or an explicit signal that indexing is still in
progress, distinguishable from "genuinely nothing found."

### Out of Scope

- Re-implementing or replacing the existing capability-based gate
  (`prepare_gated_document` capability checks stay as-is; this feature adds an
  additional readiness dimension, it does not remove the existing one).
- Building a UI/progress-bar experience for indexing — the requirement is a correct,
  distinguishable signal to the MCP caller, not a human-facing progress indicator.
- Per-server custom indexing protocols beyond what is reachable through standard LSP
  signals (`$/progress` / `WorkDoneProgress`) plus rust-analyzer's documented
  `experimental/serverStatus` extension. Bespoke integrations for other servers'
  proprietary status protocols (if any) are `[NEEDS CLARIFICATION]`, not committed scope.
- Changing `get_document_symbols` / `workspace_symbol_search` behavior — whether and how
  these are gated is an explicit open design question for the plan phase, not settled
  here.

## 2. User Stories

### US-001: Agent queries references during workspace cold start

AS A an AI coding agent using mcpls to navigate a large Rust workspace
I WANT `get_references` (and the other whole-workspace-dependent read tools) to either
wait for indexing or clearly tell me indexing is incomplete
SO THAT I do not mistake "index not ready" for "this symbol is truly unused" and make an
incorrect decision (e.g. deleting code) based on a misleading empty result

**Acceptance criteria:**
```
GIVEN mcpls has just reported the routed LSP server as ready (initialize/initialized
      handshake complete) on a large workspace whose indexing is still in progress
WHEN the agent calls get_references on a symbol with many real usages elsewhere in
     the workspace
THEN the tool response is not an unqualified empty/null result attributable to
     incomplete indexing — the response either reflects a sufficiently-indexed
     workspace, or explicitly signals that indexing is still in progress
```

### US-002: Agent queries hover/definition/completions/code-actions during cold start

AS A an AI coding agent
I WANT the same indexing-awareness behavior as US-001 applied consistently across
`get_hover`, `get_definition`, `get_completions`, and `get_code_actions`
SO THAT I get consistent, trustworthy behavior across the whole read-tool surface, not
just for references

**Acceptance criteria:**
```
GIVEN the routed LSP server is still completing its initial workspace load
WHEN the agent calls get_hover, get_definition, get_completions, or get_code_actions
THEN each tool applies the same readiness gate / explicit-signal behavior as
     get_references, appropriate to what that tool's answer depends on
```

### US-003: Agent queries a fast/small workspace with no meaningful indexing delay

AS A an AI coding agent working in a small workspace or with a language server that has
no async indexing phase
I WANT no added latency or spurious "still indexing" errors
SO THAT the fix does not regress the common case

**Acceptance criteria:**
```
GIVEN the routed LSP server has no workspace-load signal in flight (either it completed
      indexing before the request, or the server has no indexing phase at all)
WHEN the agent calls any gated read tool
THEN the tool responds with effectively the same latency as before this feature
     (no fixed artificial delay is added when the server is already ready)
```

### US-004: Agent queries a workspace that never signals readiness

AS A an AI coding agent working against an LSP server that does not implement any
recognized readiness signal (no `experimental/serverStatus`, no `$/progress` for
workspace load)
I WANT the tool to still respond within a bounded time rather than hang indefinitely
SO THAT mcpls remains usable as a universal bridge across LSP servers with varying
levels of protocol support

**Acceptance criteria:**
```
GIVEN the routed LSP server never emits a recognized workspace-readiness signal
WHEN the agent calls a gated read tool
THEN mcpls falls back to existing capability-gated behavior after a bounded timeout,
     rather than blocking indefinitely
```

## 3. Functional Requirements

Use EARS notation. Prefix with FR-NNN.

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN a read tool whose result depends on whole-workspace analysis (hover, definition, references, completions, code actions) is invoked THE SYSTEM SHALL determine whether the routed LSP server has completed its initial workspace load before answering | must |
| FR-002 | WHEN the routed LSP server has not completed its initial workspace load AND a recognized readiness signal indicates indexing is still in progress THE SYSTEM SHALL either wait (bounded, see NFR-001) for readiness or return an explicit "still indexing / not ready" condition to the MCP caller, instead of returning an unqualified empty/null result | must |
| FR-003 | WHEN the routed LSP server signals workspace-load completion (e.g. via `experimental/serverStatus` with `quiescent: true`, or the completion of a `$/progress` / `WorkDoneProgress` sequence associated with workspace load) THE SYSTEM SHALL treat the server as ready for whole-workspace-dependent queries | must |
| FR-004 | WHEN no recognized readiness signal is available for the routed LSP server (server does not implement one, or mcpls has no server-specific mapping for it) THE SYSTEM SHALL fall back to the existing capability-only gate (`prepare_gated_document`) after a bounded wait, so the server is never blocked indefinitely | must |
| FR-005 | WHEN mcpls surfaces a "still indexing / not ready" condition to the MCP caller THE SYSTEM SHALL make it structurally distinguishable from a genuine empty/no-results answer (e.g. a distinct error/status rather than the same `[]`/`null` shape) | must |
| FR-006 | WHEN the readiness-gate mechanism is implemented THE SYSTEM SHALL apply it generically across LSP servers (not hard-coded to rust-analyzer only), using server-agnostic LSP signals where available and degrading gracefully per FR-004 for servers without them | must |
| FR-007 | WHEN the routed LSP server is already fully indexed at the time of a request THE SYSTEM SHALL answer without incurring the bounded-wait timeout (no added latency in the already-ready case) | must |
| FR-008 | WHEN `get_document_symbols` or `workspace_symbol_search` is invoked THE SYSTEM SHALL apply whichever readiness treatment is decided at plan time — `[NEEDS CLARIFICATION: should get_document_symbols remain ungated per the zeenix precedent (file-local analysis is valid mid-index), and should workspace_symbol_search be gated like the whole-workspace tools since it is workspace-wide by nature?]` | should |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Performance | The bounded wait for indexing readiness has a maximum timeout value; `[NEEDS CLARIFICATION: exact timeout value — zeenix uses a configurable bound; candidates include a fixed default (e.g. 30s) with optional config override]` |
| NFR-002 | Performance | No latency regression for the already-ready / no-indexing-phase case (see FR-007, US-003) — the readiness check itself must be O(cheap state read), not a network round-trip when the state is already known ready |
| NFR-003 | Compatibility | The readiness-gate mechanism MUST be generic across LSP servers, not rust-analyzer-specific, consistent with mcpls's role as a universal MCP-to-LSP bridge (see FR-006) |
| NFR-004 | Reliability | The mechanism MUST NOT deadlock or hang indefinitely for a server that never emits a recognized readiness signal (see FR-004, US-004) |
| NFR-005 | Observability | The "still indexing" condition surfaced to the caller should be loud/clear rather than silent, mirroring the precedent set by issue #244 (silent incomplete result treated as P1) |
| NFR-006 | Testability | The production readiness-gate logic should be reusable by (or replace) the existing test-only helpers (`wait_for_indexing_ready` and hover-probe polling in the test suite), reducing duplication between test and production code paths |

## 5. Data Model

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| Workspace Readiness State | Per-LSP-server tracking of whether initial workspace indexing has completed, distinct from the existing handshake-based `ServerState` | server id/handle, indexing-complete flag or enum (unknown / in-progress / ready / no-signal-available), last-observed progress signal timestamp |
| Readiness Signal | A normalized representation of server-reported progress toward workspace-load completion, regardless of underlying LSP mechanism | signal source (`experimental/serverStatus` / `$/progress` / none), raw payload, derived ready/not-ready verdict |
| Indexing-Not-Ready Response | The explicit condition returned to an MCP caller when a gated tool is invoked before readiness is reached and the bounded wait expires without reaching ready | tool name, elapsed wait duration, reason (timeout vs. explicit "still loading" signal) |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Server never completes indexing (crashes, hangs) during the bounded wait | System falls back per FR-004 after timeout rather than hanging; the underlying server crash/hang is still handled by existing LSP lifecycle/error-recovery paths, not newly introduced by this feature |
| Server signals workspace-load completion, then later reports fresh work (e.g. re-indexing after a large file change) | `[NEEDS CLARIFICATION: does the gate need to re-arm on subsequent indexing events, or is initial-load-only sufficient for this feature's scope?]` |
| Multiple concurrent gated requests arrive while indexing is in progress | All concurrent requests observe the same readiness state; no duplicate/uncoordinated polling of the LSP server's status per request |
| Server has no indexing phase at all (e.g. a simple/stateless LSP server) | Treated as immediately ready — no wait, no "still indexing" response (see US-003) |
| `experimental/serverStatus` payload is malformed or missing expected fields | Treated as "no recognized signal" and falls back to bounded-wait + capability-gate behavior (FR-004), not a hard error |
| Bounded wait timeout is reached mid-request | Explicit "still indexing / not ready" condition returned per FR-002/FR-005, not a silent empty result and not an unrelated generic error |

## 7. Success Criteria

Measurable metrics that prove the feature works:

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Reproduction from the finding (call `get_references` on a heavily-used symbol immediately after `initialize`/`initialized` on a large workspace) | Response is no longer an unqualified empty `[]` indistinguishable from "no references" — either a populated result or an explicit not-ready signal |
| SC-002 | Latency for an already-indexed / no-indexing-phase workspace | No measurable regression vs. pre-fix baseline (see NFR-002) |
| SC-003 | Behavior against a non-rust-analyzer LSP server with no recognized readiness signal | Falls back to bounded-wait-then-capability-gate behavior without hanging (see FR-004, NFR-004) |

## 8. Agent Boundaries

### Always (without asking)
- Run the full check suite (`cargo +nightly fmt --check`, `cargo clippy --all-targets --all-features --workspace -- -D warnings`, `cargo nextest run --workspace --all-features --lib --bins`, rustdoc gate) before any commit, per project conventions
- Follow existing patterns in `bridge/translator/` and `lsp/lifecycle.rs` for state management and error handling
- Keep the readiness-gate mechanism server-agnostic at its core, with server-specific signal adapters isolated (do not special-case rust-analyzer logic throughout the general bridge code)

### Ask First
- Adding new dependencies for progress-signal parsing or timeout/async coordination beyond what's already in the workspace
- Changing the public MCP tool response schema/shape for the gated tools (the "not ready" signal shape is part of the public contract with MCP callers)
- Choosing the default timeout value (NFR-001) if no existing project convention applies

### Never
- Silently swallow the "still indexing" condition and fall back to returning an empty result without any distinguishing signal — that reproduces the exact bug this feature fixes
- Hard-code rust-analyzer-only readiness logic into the generic bridge/translator layer in a way that other LSP servers cannot opt into or gracefully fall back from
- Remove or weaken the existing capability-based gate (`prepare_gated_document`) — this feature adds a readiness dimension on top of it

## 9. Open Questions

- [NEEDS CLARIFICATION: exact readiness-signal strategy per server — which servers beyond rust-analyzer have a usable standard `$/progress`/`WorkDoneProgress` workspace-load sequence mcpls can rely on generically, versus needing server-specific adapters or having no signal at all?] — tracked in #422.
- [NEEDS CLARIFICATION: exact bounded-wait timeout value — fixed default vs. configurable, and what default (candidate: 30s, matching the order of magnitude in the finding's reproduction)?] — resolved: fixed 30s default, see Resolution.
- [NEEDS CLARIFICATION: should `get_document_symbols` remain ungated per the zeenix precedent (file-local analysis valid mid-index)?] — resolved: yes, see Resolution.
- [NEEDS CLARIFICATION: should `workspace_symbol_search` be gated like the whole-workspace tools (it is workspace-wide by nature) or treated separately?] — deferred, tracked in #422.
- [NEEDS CLARIFICATION: does the readiness gate need to re-arm after the initial load (e.g. on large-scale re-indexing triggered by bulk file changes), or is "initial workspace load only" sufficient scope for this feature?]
- [NEEDS CLARIFICATION: should the production readiness-gate implementation replace the existing test-only helpers (`wait_for_indexing_ready`, hover-probe polling) entirely, or can the test helpers keep their own polling for now and only share the underlying signal-detection logic?]

## 10. See Also

- [[constitution]] — project principles
- [[MOC-specs]] — all specifications
- [zeenix/rust-analyzer-mcp commit a772c827](https://github.com/zeenix/rust-analyzer-mcp/commit/a772c827) — reference fix for the same bug class in a comparable rust-analyzer MCP bridge
- mcpls issue #121 — test-only readiness-gate history
- mcpls issue #127 — test-only readiness-gate history
- mcpls issue #244 — precedent for treating silent-incomplete-result bugs as P1 (`get_diagnostics` silently omitting cargo-check/flycheck warnings)
- mcpls issue #422 — follow-up: generic `$/progress`-based indexing detection for non-rust-analyzer servers
- [LSP 3.17 Specification — Work Done Progress](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#workDoneProgress) — generic `$/progress` / `WorkDoneProgress` signal source
- rust-analyzer `experimental/serverStatus` notification — rust-analyzer-specific extension signal source for workspace-load/indexing completion
