---
aliases:
  - Project Principles
tags:
  - sdd
  - constitution
created: 2026-09-06
status: permanent
---

# Project Constitution

> [!important]
> Non-negotiable principles governing ALL development in this project.
> Every specification, plan, and task MUST comply with this document.
> Update this file only through explicit team decision.

## I. Architecture

**mcpls** is a bridge between MCP (Model Context Protocol) and LSP (Language Server Protocol):

```
AI Client (Claude) ←→ [MCP] ←→ mcpls ←→ [LSP] ←→ rust-analyzer / pyright / ...
```

The codebase is organized into four modules that match the four-block spec numbering scheme in `specs/`:

- **`config/`** — TOML config loading, LSP server discovery with project-marker heuristics (Cargo.toml → rust-analyzer, package.json → typescript-language-server, etc.), auto-generates default config with 30 language mappings on first run
- **`lsp/`** — LSP client: spawns server processes, manages JSON-RPC 2.0 over stdin/stdout, handles lifecycle (initialize → shutdown)
- **`mcp/`** — MCP server (via `rmcp` crate): defines 20 tools (hover, definition, references, diagnostics, rename, format, call hierarchy, signature help, implementation/type-definition, inlay hints, etc.), dispatches tool calls to the bridge
- **`bridge/`** — protocol translation layer:
  - `translator/` — core MCP→LSP request/response mapping (organized into submodules: diagnostics, navigation, routing, symbols, call_hierarchy, assist, edits, respawn, characterization, clock, dto, encoding_ctx)
  - `state.rs` — lazy document state tracking (textDocument/didOpen sent on first access)
  - `encoding.rs` — position conversion: MCP uses 1-based lines/columns, LSP uses 0-based
  - `notifications.rs` — caches push-based LSP notifications (diagnostics, log messages) for polling via MCP tools

Cross-cutting concerns (CLI argument parsing, signal handling, transport-level shutdown) live in `mcpls-cli` and `mcpls-core`'s top-level modules under the `runtime` block.

**Non-negotiable design constraints:**

- Position encoding mismatch: MCP is 1-based, LSP is 0-based — all conversions go through `bridge/encoding.rs` (critical path; every tool depends on this)
- Diagnostics are push-based in LSP but poll-based in MCP — `notifications.rs` caches them for safe querying
- Multiple LSP servers run concurrently; one failing must not affect others (graceful degradation — a design principle, not a feature)
- `deny(unsafe_code)` enforced workspace-wide via `unsafe_code = "deny"` in `workspace.lints`

## II. Technology Stack

- **Language**: Rust 1.88+ (MSRV enforced in `Cargo.toml` as `rust-version = "1.88"`)
- **Edition**: 2024 edition
- **Primary dependencies**:
  - `tokio` 1.53+ — async runtime
  - `rmcp` 3.2.0 — MCP server framework
  - `lsp-types` (gen-lsp-types) 0.11.0 — LSP type definitions (fixed version to ensure stable protocol surface)
  - `serde` 1.0 — serialization
  - `anyhow` 1.0 — error handling
  - `clap` 4.6 — CLI argument parsing
  - `toml` 1.1 — configuration file parsing
  - `tracing` 0.1 — structured logging

No in-memory database; no external services required for core functionality (LSP servers are spawned as child processes, not external services).

## III. Testing (NON-NEGOTIABLE)

All features must have tests that pass before merge. Testing standards:

- **Framework**: `cargo nextest` for parallel test execution; `rstest` for parametrized tests
- **Minimum coverage**: All new public APIs and bridge translation logic require tests
- **Integration testing**: End-to-end stack tests preferred over mocks for critical paths (position encoding, document tracking, notification caching)
- **Pre-commit checklist**: All of the following must pass:
  ```bash
  cargo +nightly fmt --all -- --check
  cargo clippy --all-targets --all-features --workspace -- -D warnings
  cargo nextest run --workspace --all-features --lib --bins
  RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
  ```

## IV. Code Style

- Follow existing patterns in the codebase (refer to `crates/mcpls-core/src/` subdirectories for established idioms)
- All public APIs (`pub fn`, `pub struct`, `pub trait`, etc.) must have doc comments (`///`) with `# Examples` for non-trivial APIs
- Doc comments must explain WHAT and WHY, not restate the signature
- Avoid `unwrap()` and `expect()` in production code — use proper error handling (`Result`, `anyhow::Context`, or explicit fallback)
- No hand-written UTF-8/UTF-16/UTF-32 encoding logic outside `bridge/encoding.rs` — reuse the conversion helpers
- Follow the lock-ordering discipline documented in `crates/mcpls-core/src/bridge/translator/diagnostics.rs` comments around `NotificationCache` access

## V. Security

- Never commit secrets, keys, credentials, or API tokens
- All user input (file paths, LSP server responses, MCP client requests) must be validated at system boundaries
- File path handling must use `dunce` for UNC path normalization and validate against symlink escapes where applicable
- Unsafe code blocks must have a safety comment explaining the invariant and why it cannot panic
- Dependency security: maintain `cargo deny` checks in CI (no unaudited or yanked versions)

## VI. Performance

No hard latency targets — performance is secondary to correctness and simplicity. However:

- Position encoding / document tracking / notification caching are on every critical path; optimization here is justified
- Merge/dedup logic for diagnostics must be O(n) or O(n log n) in the number of diagnostics for a file (typically single/low double digits)
- No additional LSP round-trips added to existing tool flows (e.g., don't spawn a new diagnostic request just to fix an incomplete result)

## VII. Simplicity

- Prefer standard library and framework features over third-party alternatives
- New dependencies require justification (document in CHANGELOG.md and PR description)
- Avoid multiple approaches to the same problem — establish one pattern and reuse it (example: position encoding logic is centralized in `bridge/encoding.rs`, not scattered across tool handlers)
- Remove obsolete code paths entirely rather than deprecating or gating them behind flags
- Module-level documentation (`//!`) must explain responsibility and fit into the architecture

## VIII. Git Workflow

**Branch naming:**
- Features: `feat/m{N}/{issue-number}-{feature-slug}` (e.g., `feat/m3/42-auth-module`)
- Bug fixes: `fix/{issue-number}-{short-slug}` (e.g., `fix/58-null-pointer`)
- Hotfixes: `hotfix/{issue-number}-{short-slug}`
- If no issue, omit the number: `feat/{feature-slug}` or `fix/{short-slug}`

**Commit messages:** Follow Conventional Commits 1.0.0 format:
```
<type>[optional scope]: <description>

[optional body]

[optional footer(s)]
```

Allowed types: `feat`, `fix`, `docs`, `style`, `refactor`, `test`, `build`, `ci`, `perf`, `chore`, `release`. Each maps to semver as specified in `.claude/rules/commits-and-issues.md`.

**Anti-patterns:**
- Do not use past tense ("added feature" → use "add feature")
- Do not mention AI tools, co-authors, or code generation
- Do not use emoji
- No session links in commit messages

**Before creating a PR:** Run the full pre-commit check suite listed in Section III (Testing) — local checks must match CI exactly.

**Issue filing:** All issues must have a priority label (P0–P4) as an actual label, plus a category (`bug`, `enhancement`, `research`). See `.claude/rules/commits-and-issues.md` for severity definitions and issue templates.
