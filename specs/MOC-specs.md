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

## lsp

| # | Slug | Type | Priority | Status | Issue |
|---|------|------|----------|--------|-------|
| 001 | [[lsp/001-lsp-server-lifecycle-and-respawn/spec\|lsp-server-lifecycle-and-respawn]] | enhancement | P1 | implemented (retroactive) | — |
| 002 | [[lsp/002-lsp317-missing-tools/spec\|lsp317-missing-tools]] | enhancement | P3 | draft (tools now implemented — see [[testing/001-e2e-rust-analyzer-testing/spec\|testing/001]]'s Resolution; this spec's own status is stale, see note below) | #116 |
| 003 | [[lsp/003-lsp-types-unmaintained-migration/spec\|lsp-types-unmaintained-migration]] | research | P2 | draft | #297 |
| 004 | [[lsp/004-lsp-318-draft-gaps/spec\|lsp-318-draft-gaps]] | research | P4 | draft | #299 (also #116; #290 resolved by #289/#291) |

## mcp

| # | Slug | Type | Priority | Status | Issue |
|---|------|------|----------|--------|-------|
| 001 | [[mcp/001-mcp-tool-surface-and-routing/spec\|mcp-tool-surface-and-routing]] | enhancement | P1 | implemented (retroactive) | — |
| 002 | [[mcp/002-mcp-resources-diagnostics/spec\|mcp-resources-diagnostics]] | enhancement | P3 | draft | #115 |
| 003 | [[mcp/003-mcp-2026-stateless-adoption/spec\|mcp-2026-stateless-adoption]] | research | P3 | draft | #298 |

## bridge

| # | Slug | Type | Priority | Status | Issue |
|---|------|------|----------|--------|-------|
| 001 | [[bridge/001-position-encoding-layer/spec\|position-encoding-layer]] | enhancement | P1 | implemented (retroactive) | — |
| 002 | [[bridge/002-document-tracker-synchronization/spec\|document-tracker-synchronization]] | enhancement | P1 | implemented (retroactive) | — |
| 003 | [[bridge/003-rwlock-translator/spec\|rwlock-translator]] | enhancement | P2 | draft | #114 |
| 004 | [[bridge/004-get-diagnostics-flycheck-gap/spec\|get-diagnostics-flycheck-gap]] | bug | P1 | draft | — |
| 005 | [[bridge/005-expose-document-tracker-limits/spec\|expose-document-tracker-limits]] | enhancement | P2 | implemented (#324) | — |

## runtime

Cross-cutting: `mcpls-cli` argument/env parsing and process-level signal/transport shutdown —
neither belongs to a single `config`/`lsp`/`mcp`/`bridge` module.

| # | Slug | Type | Priority | Status | Issue |
|---|------|------|----------|--------|-------|
| 001 | [[runtime/001-log-json-bool-env-parsing/spec\|log-json-bool-env-parsing]] | bug | P2 | implemented (#314) | — |
| 002 | [[runtime/002-sigterm-stdin-blocking-pool-hang/spec\|sigterm-stdin-blocking-pool-hang]] | bug | P1 | implemented (#321, #328) | — |

## testing

Cross-cutting: test-infrastructure specs exercising the whole MCP→mcpls→LSP stack rather than one
subsystem.

| # | Slug | Type | Priority | Status | Issue |
|---|------|------|----------|--------|-------|
| 001 | [[testing/001-e2e-rust-analyzer-testing/spec\|e2e-rust-analyzer-testing]] | enhancement | P2 | implemented (#125, #126, #139, #225) | — |

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
> order, then research/tracking specs last. `config` has only one spec today — a real reflection of
> the `config/` module's spec coverage prior to this pass, not a placeholder.
>
> Numbers are **block-scoped**, not global: `bridge/001` and `lsp/001` are different, unrelated
> specs. Always cite a spec with its block prefix (e.g. `bridge/001`, never bare `001`).

> [!warning] Known staleness: lsp/002 (formerly 003) vs. shipped code
> [[lsp/002-lsp317-missing-tools/spec|lsp/002]]'s own file still says `**Status**: draft`, but all
> four tools it proposes (`get_signature_help`, `go_to_implementation`, `go_to_type_definition`,
> `get_inlay_hints`) are already implemented in `crates/mcpls-core/src/mcp/server.rs` and exercised
> by [[testing/001-e2e-rust-analyzer-testing/spec|testing/001]]'s e2e suite. This migration did not
> rewrite lsp/002's content (out of scope for a migration/reorganization pass — see the migration
> report), but a human reviewer should follow up with a proper Resolution callout identifying the
> implementing PR(s), consistent with how `bridge/005`/`runtime/001`/`runtime/002` already document
> their own resolutions.
