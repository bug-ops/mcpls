# Map of Content — Specs

Specs are organized into blocks matching the crate's own module boundaries
(`config/`, `lsp/`, `mcp/`, `bridge/` — see the crate layout in the project's `CLAUDE.md`), plus
two cross-cutting blocks that don't belong to a single module: `runtime` (CLI argument parsing,
process signal handling, transport-level shutdown — spans `mcpls-cli` and `mcpls-core`'s top-level
`lib.rs`/`transport.rs`) and `testing` (test-infrastructure specs that exercise the whole stack
rather than one subsystem). Numbering restarts at 001 within each block.

## config

| # | Slug | Type | Priority | Status | Issue |
|---|------|------|----------|--------|-------|
| 001 | [[config/001-config-discovery-and-heuristics/spec\|config-discovery-and-heuristics]] | enhancement | P1 | implemented (retroactive) | — |
| 002 | [[config/002-typescript-7-native-server-support/spec\|typescript-7-native-server-support]] | bug | P1 | implemented | #615, #634 |
| 003 | [[config/003-unsupported-file-pattern-forms/spec\|unsupported-file-pattern-forms]] | bug | P3 | approved (implemented, #669) | #669 |

## lsp

| # | Slug | Type | Priority | Status | Issue |
|---|------|------|----------|--------|-------|
| 001 | [[lsp/001-lsp-server-lifecycle-and-respawn/spec\|lsp-server-lifecycle-and-respawn]] | enhancement | P1 | implemented (retroactive) | — |
| 002 | [[lsp/002-lsp317-missing-tools/spec\|lsp317-missing-tools]] | enhancement | P3 | implemented (#124) | #116 |
| 003 | [[lsp/003-lsp-types-unmaintained-migration/spec\|lsp-types-unmaintained-migration]] | research | P2 | implemented (#375) | #297 |
| 004 | [[lsp/004-lsp-318-draft-gaps/spec\|lsp-318-draft-gaps]] | research | P3 (bumped from P4, 2026-09-21, SC-003: LSP 3.18 finalized) | draft | #299 (also #116, resolved by #124; #290 resolved by #289/#291); #477 (SC-003 re-assessment, resolved) |
| 005 | [[lsp/005-lsp-content-modified-retry/spec\|lsp-content-modified-retry]] | bug | P2 | implemented (#390) | #382 |
| 006 | [[lsp/006-server-spawn-install-hint/spec\|server-spawn-install-hint]] | research | P3 | implemented (#530) | #494 |
| 007 | [[lsp/007-lsp-child-process-lifetime/spec\|lsp-child-process-lifetime]] | enhancement | P2 | implemented (#546) | #526 |
| 008 | [[lsp/008-lsp317-method-coverage-gaps/spec\|lsp317-method-coverage-gaps]] | enhancement | P3 (declaration), P4 (rest) | implemented (declaration #567, type hierarchy, prepare rename, highlights, range formatting); semanticTokens and others non-goals | #567, #568, #569 |
| 009 | [[lsp/009-incremental-server-registration/spec\|incremental-server-registration]] | bug | P2 | implemented | #572; #588, #589 (FR-019, FR-021) |
| 010 | [[lsp/010-workspace-configuration-push/spec\|workspace-configuration-push]] | bug | P1 | implemented | #578; #598 (settings push and serve) |
| 011 | [[lsp/011-selection-folding-range-tools/spec\|selection-folding-range-tools]] | enhancement | P4 | implemented | #616; #642 (deep response recovery) |
| 012 | [[lsp/012-client-publish-diagnostics-capability/spec\|client-publish-diagnostics-capability]] | bug | P1 | implemented | #665 |
| 013 | [[lsp/013-diagnostics-publish-burst-drops/spec\|diagnostics-publish-burst-drops]] | bug | P2 | implemented (per-client publish mailbox, lost files read evicted) | #704 |

## mcp

| # | Slug | Type | Priority | Status | Issue |
|---|------|------|----------|--------|-------|
| 001 | [[mcp/001-mcp-tool-surface-and-routing/spec\|mcp-tool-surface-and-routing]] | enhancement | P1 | implemented (retroactive) | — |
| 002 | [[mcp/002-mcp-resources-diagnostics/spec\|mcp-resources-diagnostics]] | enhancement | P3 | draft | #115; #468 (FR-006, FR-007); #521 (FR-008); #535 (FR-009); #544, #545 (FR-009..FR-012) |
| 003 | [[mcp/003-mcp-2026-stateless-adoption/spec\|mcp-2026-stateless-adoption]] | research | P3 | draft | #298; #493 (stateless subscriptions, closed as research); #522 (subscriptions/listen implemented); #593 (paced replay, bounded eviction record) |
| 004 | [[mcp/004-mcp-tasks-sep2663-adoption/spec\|mcp-tasks-sep2663-adoption]] | research | P4 | draft | #119 |
| 005 | [[mcp/005-tool-capability-discoverability/spec\|tool-capability-discoverability]] | research | P4 | implemented (#540) | #461 |
| 006 | [[mcp/006-http-stream-liveness/spec\|http-stream-liveness]] | enhancement | P3 | implemented | #543 |
| 007 | [[mcp/007-symbol-name-addressing/spec\|symbol-name-addressing]] | research | P2 | implemented (MVP, signature help deferred) | #563 |
| 008 | [[mcp/008-manual-lsp-server-restart/spec\|manual-lsp-server-restart]] | enhancement | P2 | implemented | #564 |
| 009 | [[mcp/009-speculative-edit-diagnostic-preview/spec\|speculative-edit-diagnostic-preview]] | research | P4 | decided (non-goal, verify loop documented) | #570 |
| 010 | [[mcp/010-http-session-keepalive-with-live-stream/spec\|http-session-keepalive-with-live-stream]] | enhancement | P3 | implemented | #573 |
| 011 | [[mcp/011-client-path-boundary-parsing/spec\|client-path-boundary-parsing]] | bug | P2 | implemented (#580) | #575 |
| 012 | [[mcp/012-http-typed-limits-origins-stream-deadline/spec\|http-typed-limits-origins-stream-deadline]] | enhancement | P3 | implemented | #584, #585, #587; #597, #600, #602 |
| 013 | [[mcp/013-http-allowed-host-default-port-rejection/spec\|http-allowed-host-default-port-rejection]] | enhancement | P3 | implemented | #629 |
| 014 | [[mcp/014-tools-list-payload-size/spec\|tools-list-payload-size]] | enhancement | P3 | implemented (135,500 B tools/list budget, #654 kind filter schemas, #705 closed input schemas) | #630; #654 |
| 015 | [[mcp/015-request-cancellation-propagation/spec\|request-cancellation-propagation]] | enhancement | P3 | draft | #687 |
| 016 | [[mcp/016-reject-unknown-tool-arguments/spec\|reject-unknown-tool-arguments]] | enhancement | P3 | implemented | #705 |
| 017 | [[mcp/017-bounded-client-string-echoes/spec\|bounded-client-string-echoes]] | enhancement (hardening) | P3 | draft | #749 |

## bridge

| # | Slug | Type | Priority | Status | Issue |
|---|------|------|----------|--------|-------|
| 001 | [[bridge/001-position-encoding-layer/spec\|position-encoding-layer]] | enhancement | P1 | implemented (retroactive) | — |
| 002 | [[bridge/002-document-tracker-synchronization/spec\|document-tracker-synchronization]] | enhancement | P1 | implemented (retroactive) | — |
| 003 | [[bridge/003-rwlock-translator/spec\|rwlock-translator]] | enhancement | P2 | superseded | #114 |
| 004 | [[bridge/004-get-diagnostics-flycheck-gap/spec\|get-diagnostics-flycheck-gap]] | bug | P1 | draft | — |
| 005 | [[bridge/005-expose-document-tracker-limits/spec\|expose-document-tracker-limits]] | enhancement | P2 | implemented (#324) | — |
| 006 | [[bridge/006-lsp-indexing-readiness-gate/spec\|lsp-indexing-readiness-gate]] | bug | P1 | implemented (#421) | #420 |
| 007 | [[bridge/007-enclosing-symbol-context/spec\|enclosing-symbol-context]] | research | P4 | implemented | #565 |
| 008 | [[bridge/008-workspace-root-configured-spelling/spec\|workspace-root-configured-spelling]] | bug (regression of #533/#552) | P1 | implemented (#580); root-level system symlink aliases (#579) | #571, #579 |
| 009 | [[bridge/009-diagnostics-subscription-staleness/spec\|diagnostics-subscription-staleness]] | enhancement | P3 | implemented | #574; #648, #649 (follow-ups) |
| 010 | [[bridge/010-workspace-containment-single-predicate/spec\|workspace-containment-single-predicate]] | refactor | P2 | implemented (#580) | #558 |
| 011 | [[bridge/011-push-only-server-diagnostics/spec\|push-only-server-diagnostics]] | bug | P2 | draft (implemented, #666, #670) | #666; #670 |
| 012 | [[bridge/012-indexing-gate-after-restart/spec\|indexing-gate-after-restart]] | bug | P1 | draft (implemented, #667) | #667 |
| 013 | [[bridge/013-indexing-state-on-ungated-prepare-tools/spec\|indexing-state-on-ungated-prepare-tools]] | enhancement | P3 | draft (implemented, #668) | #668 |
| 014 | [[bridge/014-stale-push-entry-merged-with-fresh-pull/spec\|stale-push-entry-merged-with-fresh-pull]] | bug | P2 | implemented (coverage-based exclusion, learned pull sources) | #703 |

## runtime

Cross-cutting: `mcpls-cli` argument/env parsing and process-level signal/transport shutdown —
neither belongs to a single `config`/`lsp`/`mcp`/`bridge` module.

| # | Slug | Type | Priority | Status | Issue |
|---|------|------|----------|--------|-------|
| 001 | [[runtime/001-log-json-bool-env-parsing/spec\|log-json-bool-env-parsing]] | bug | P2 | implemented (#314) | — |
| 002 | [[runtime/002-sigterm-stdin-blocking-pool-hang/spec\|sigterm-stdin-blocking-pool-hang]] | bug | P1 | implemented (#321, #328) | — |
| 003 | [[runtime/003-workspace-supplied-code-execution/spec\|workspace-supplied-code-execution]] | research | P3 | implemented (tsserver pin, docs, SECURITY.md, untrusted-workspace mode, launcher refusals, untrusted working directory) | #566; #652, #653, #657 |
| 004 | [[runtime/004-server-text-hygiene/spec\|server-text-hygiene]] | bug | P3 | implemented | #581, #582, #583; #599 |
| 005 | [[runtime/005-untrusted-unlisted-wrapper-workspace-program/spec\|untrusted-unlisted-wrapper-workspace-program]] | enhancement | P3 | implemented (#724) | #724 |

## testing

Cross-cutting: test-infrastructure specs exercising the whole MCP→mcpls→LSP stack rather than one
subsystem.

| # | Slug | Type | Priority | Status | Issue |
|---|------|------|----------|--------|-------|
| 001 | [[testing/001-e2e-rust-analyzer-testing/spec\|e2e-rust-analyzer-testing]] | enhancement | P2 | implemented (#125, #126, #139, #225) | — |

## Project Foundation

- [[constitution]] — non-negotiable project principles governing all specs

> [!note] Block assignment and numbering rationale
> Every spec was reassigned to the block its subject matter is *most fundamentally about*, not
> necessarily where its implementation happens to live in the crate tree. Two calls worth flagging:
> - `mcp/001-mcp-tool-surface-and-routing` documents `ToolRouter`/`ToolKind`/`ServerId`, whose code
>   physically lives in `crates/mcpls-core/src/config/routing.rs` (kept there deliberately to avoid
>   a `config → mcp → bridge → config` dependency cycle, per that file's own module doc). The spec
>   is filed under `mcp` because its subject — which MCP tool call reaches which server — is an
>   MCP-facing concern; its `related:` frontmatter and prose cross-link `config/001` for the
>   config-side data it's built from.
> - `lsp/002-lsp317-missing-tools` is filed under `lsp` (not `mcp`, even though its deliverable is 4
>   new MCP tools) because its own content frames it as LSP-protocol-capability parity tracking
>   (comparing mcpls against LSP spec versions and reference projects), matching the placement of
>   its direct sibling `lsp/004-lsp-318-draft-gaps`.
>
> Within each block, specs are ordered: any new retroactive foundational spec first (documents the
> subsystem itself), then the original historical bug/enhancement specs in their original relative
> order, then research/tracking specs last. `config` has three specs today — a real reflection of
> the `config/` module's spec coverage, not a placeholder.
>
> Numbers are **block-scoped**, not global: `bridge/001` and `lsp/001` are different, unrelated
> specs. Always cite a spec with its block prefix (e.g. `bridge/001`, never bare `001`).
