---
aliases:
  - Config discovery and project heuristics
  - mcpls.toml loading tiers
tags:
  - sdd
  - spec
  - config
created: 2026-08-05
status: implemented
related:
  - "[[constitution]]"
  - "[[mcp/001-mcp-tool-surface-and-routing/spec]]"
  - "[[lsp/001-lsp-server-lifecycle-and-respawn/spec]]"
  - "[[bridge/001-position-encoding-layer/spec]]"
---

# Feature: Configuration Loading, Multi-Tier Discovery, and Project-Marker Heuristics

> [!info] Metadata
> **Author**: retroactive spec, authored during the `.local/specs/` → `specs/` migration and gap
> analysis (no single originating commit/PR — this documents already-shipped, working
> functionality accreted across many PRs, several referenced by number in the source itself:
> #267, #279, #285, #290/#291, #297, #309, #314, #315/#293/#324, #345, #348, #359, #165, #174)
> **Type**: core subsystem
> **Priority**: P1 (retroactive — reflects centrality, not an open defect)

> [!success] Resolution
> This is a retroactive spec: `crates/mcpls-core/src/config/mod.rs` and
> `crates/mcpls-core/src/config/server.rs` already implement everything described below.
> Representative evidence: `ServerConfig::load`/`load_with_trust`/`load_from` (config/mod.rs),
> `ServerHeuristics::is_applicable_recursive` (config/server.rs), and the default 6-server /
> ~30-extension built-in config (`ServerConfig::default`, `default_language_extensions`). No
> single PR authored this end-to-end; it is the accretion of the config subsystem's entire
> history, most recently including the untrusted-project-config model (#345/#348) and the bounded
> config-file read (#309).

## 1. Overview

### Problem Statement

mcpls needs to answer three independent questions before it can spawn any LSP server:

1. **Where is the config?** A user may supply `$MCPLS_CONFIG`/`--config` explicitly, rely on an
   auto-discovered project-local `./mcpls.toml`, fall back to a per-user config directory, or run
   with no config file at all (built-in defaults, auto-created on first run).
2. **Should a project-local config be trusted?** A `./mcpls.toml` discovered relative to the
   process's *current working directory* can be planted by whoever controls a checked-out
   repository — unlike an explicitly-named `--config`/`$MCPLS_CONFIG` path, naming which is itself
   the user's consent. A malicious project-local config could redirect spawned commands
   (`lsp_servers[].command`/`args`), redirect the effective workspace (`workspace.roots`), or drive
   a filesystem-walk denial-of-service (`workspace.heuristics_max_depth`).
3. **Which LSP servers actually apply to this workspace?** mcpls ships ~30 built-in file-extension
   → language-ID mappings and 6 built-in server configs (rust-analyzer, pyright,
   typescript-language-server, gopls, clangd, zls), each gated by project-marker heuristics
   (`Cargo.toml` → rust-analyzer, `package.json` → typescript-language-server, etc.) so mcpls
   doesn't attempt to spawn rust-analyzer inside a pure-Python repository.

Getting any of these wrong has a real blast radius: an untrusted config trusted by default is a
supply-chain / arbitrary-command-execution risk; an over-broad or too-narrow heuristic either
spawns servers that will never successfully analyze anything, or fails to spawn a server the
workspace actually needs.

### Goal

mcpls resolves its effective configuration deterministically across four tiers (explicit
`$MCPLS_CONFIG`/`--config` → project-local `./mcpls.toml` (opt-in trust) → per-user config
directory → built-in defaults), validates it before use, and applies project-marker heuristics
(recursive, depth-bounded, exclusion-aware) to decide which of the configured LSP servers actually
apply to a given workspace — all without requiring any config file to exist at all for a
reasonable out-of-the-box experience.

### Out of Scope

- Per-tool routing once multiple servers are configured for one language — that is
  [[mcp/001-mcp-tool-surface-and-routing/spec|spec mcp/001]]'s `ToolRouter`, a distinct, later-added layer
  (#174) that consumes this spec's `LspServerConfig`/`ServerHeuristics` output.
- `workspace.max_documents`/`max_file_size` (resource limits) — already covered by
  [[bridge/005-expose-document-tracker-limits/spec|spec bridge/005]].
- `workspace.position_encodings` negotiation itself — covered by
  [[lsp/001-lsp-server-lifecycle-and-respawn/spec|spec lsp/001]] (`LspServer::spawn`) and
  [[bridge/001-position-encoding-layer/spec|spec bridge/001]] (the conversion math); this spec covers only that
  the field is loaded, validated, and defaulted.

## 2. User Stories

### US-001: Zero-config first run

AS A new mcpls user with no `mcpls.toml` anywhere
I WANT mcpls to start with sensible built-in defaults (6 popular language servers, ~30 extension
mappings) and write a default config file to my user config directory
SO THAT I get useful behavior immediately and have something to edit later

**Acceptance criteria:**
```
GIVEN no mcpls.toml exists at $MCPLS_CONFIG, ./mcpls.toml, or the platform user-config path
WHEN ServerConfig::load() runs
THEN it returns the built-in default config AND writes that default config to the user-config
     path for future edits (best-effort — a write failure logs a warning and still returns
     in-memory defaults rather than failing startup)
```

### US-002: A checked-out repository does not silently execute an attacker-controlled command

AS A developer opening someone else's repository that happens to contain a `./mcpls.toml`
I WANT that project-local config ignored by default, with a clear warning naming the exact path
and how to opt in
SO THAT cloning and running mcpls against an unfamiliar repository cannot silently execute an
arbitrary command that config names

**Acceptance criteria:**
```
GIVEN a ./mcpls.toml exists in the current working directory
  AND the caller has not passed --trust-project-config / MCPLS_TRUST_PROJECT_CONFIG=true
WHEN ServerConfig::load() (equivalently load_with_trust(Untrusted)) runs
THEN the project-local file is skipped entirely (including its [workspace] section), a warning
     names the ignored path and the opt-in flag, and project_config_ignored is set to true so an
     MCP client can also see the ignore decision in-band
```

### US-003: A monorepo only spawns the servers its subprojects actually need

AS A developer working in a monorepo containing Rust, Python, and TypeScript subprojects
I WANT each configured LSP server to spawn only if its project markers exist somewhere in the
workspace tree (not just the root), while excluding well-known noise directories
SO THAT mcpls doesn't waste resources spawning irrelevant servers, and doesn't miss a nested
subproject's server because the marker isn't at the workspace root

**Acceptance criteria:**
```
GIVEN a workspace root containing no Cargo.toml at the root but a nested packages/rust-lib/Cargo.toml
WHEN LspServerConfig::should_spawn is evaluated for the rust-analyzer server config
THEN it returns true (recursive marker search finds the nested Cargo.toml, up to
     heuristics_max_depth)

GIVEN a node_modules/some-package/package.json buried inside the workspace
WHEN the typescript-language-server heuristic searches for package.json
THEN node_modules is excluded from the search and this nested package.json does not trigger a
     false-positive spawn
```

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | THE SYSTEM SHALL resolve configuration in this precedence order: (1) `$MCPLS_CONFIG` env var, always trusted; (2) project-local `./mcpls.toml`, trusted only if the caller opts in; (3) platform user-config directory (`~/.config/mcpls/mcpls.toml` or platform equivalent); (4) built-in defaults | must |
| FR-002 | WHEN no config file exists at any tier THE SYSTEM SHALL return the built-in default `ServerConfig` and attempt to write it to the user-config path for future discovery, without failing startup if the write fails | must |
| FR-003 | WHEN a project-local `./mcpls.toml` is found and trust is `Untrusted` (the default) THE SYSTEM SHALL skip it entirely (not partially — including `[workspace]`), log a warning naming the resolved path and the opt-in mechanism, and set `project_config_ignored: true` on the returned config | must |
| FR-004 | WHEN a project-local `./mcpls.toml` is found and trust is `Trusted` THE SYSTEM SHALL load it via the same path/validation logic as an explicitly-named config | must |
| FR-005 | THE SYSTEM SHALL bound every config-file read to `MAX_CONFIG_FILE_BYTES` (8 MiB) via a `Read::take`-bounded read, not a `metadata().len()` pre-check alone, since the latter is bypassable by character devices/FIFOs reporting `len() == 0` regardless of actual readable data | must |
| FR-006 | THE SYSTEM SHALL validate every loaded (or caller-constructed) `ServerConfig` via `ServerConfig::validate()` before it is used by `serve`/`serve_with`, rejecting the first violated rule with a diagnosable `Error::InvalidConfig` | must |
| FR-007 | THE SYSTEM SHALL resolve relative `workspace.roots` entries against the config file's own directory for an explicitly-named config (`load_from`'s default), but against the process's current working directory for the auto-discovered global/user config tier (since that tier is not tied to any particular project) | must |
| FR-008 | THE SYSTEM SHALL provide 6 built-in `LspServerConfig`s (rust-analyzer, pyright, typescript-language-server, gopls, clangd, zls), each gated by `ServerHeuristics::project_markers` naming the files/directories that indicate that language's project type | must |
| FR-009 | THE SYSTEM SHALL provide ~30 built-in file-extension → language-ID mappings (`default_language_extensions`), user-overridable/-extensible via `workspace.language_extensions` | must |
| FR-010 | WHEN `ServerHeuristics::is_applicable_recursive` searches a workspace tree for project markers THE SYSTEM SHALL search recursively up to `heuristics_max_depth` (default 10), excluding well-known noise directories (`node_modules`, `target`, `.git`, `__pycache__`, `.venv`, `venv`, `.tox`, `.mypy_cache`, `.pytest_cache`, `build`, `dist`, `.cargo`, `.rustup`, `vendor`, `coverage`, `.next`, `.nuxt`) | must |
| FR-011 | WHEN `ServerHeuristics::project_markers` is empty THE SYSTEM SHALL treat the server as always applicable (no heuristic gating) | must |
| FR-012 | THE SYSTEM SHALL reject (`Error::InvalidConfig`) an empty `workspace.position_encodings` list, an unrecognized encoding string, an empty `workspace.roots` entry, an empty/duplicate-claiming server config (`language_id`, `command`, `handles`), and a `timeout_seconds`/`request_timeout_seconds` of `0` or above `MAX_TIMEOUT_SECONDS` (900s) | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Security | A project-local `./mcpls.toml`'s trust decision must default to the safer option (`Untrusted`) — an opt-in, not opt-out, security posture, since it can redirect spawned commands and workspace roots |
| NFR-002 | Security | A config file read must never be able to block indefinitely or exhaust memory regardless of what the path points at (regular file, special device, symlink to either) — enforced by the bounded read (FR-005), not a size pre-check alone |
| NFR-003 | Robustness | Heuristic marker search must never descend into pathological directory trees (`node_modules`, build output, VCS internals) that could make a recursive walk prohibitively slow or effectively unbounded |
| NFR-004 | Compatibility | A config file predating a newly-added field with a serde default (e.g. `request_timeout_seconds` added after `timeout_seconds`) must still load correctly, defaulting the new field rather than failing to parse |
| NFR-005 | Diagnosability | Every `Error::InvalidConfig` message must name the specific field and value that failed validation, not a generic "invalid config" message |

## 5. Data Model

| Entity | Description | Key Attributes |
|--------|-------------|-----------------|
| `ServerConfig` | Top-level loaded configuration | `mcp: McpConfig`, `workspace: WorkspaceConfig`, `lsp_servers: Vec<LspServerConfig>`, `project_config_ignored: bool` (load-time metadata, not user-configurable) |
| `WorkspaceConfig` | Workspace-scoped settings | `roots`, `position_encodings`, `language_extensions`, `heuristics_max_depth`, `max_documents`, `max_file_size` |
| `LspServerConfig` | One configured LSP server | `language_id`, `command`, `args`, `env`, `file_patterns`, `initialization_options`, `timeout_seconds`, `request_timeout_seconds`, `heuristics: Option<ServerHeuristics>`, `name: Option<String>`, `handles: Option<Vec<ToolKind>>` |
| `ServerHeuristics` | Project-marker gating for one server | `project_markers: Vec<String>` (empty = always applicable) |
| `ProjectConfigTrust` | Trust decision for a CWD-discovered `./mcpls.toml` | `Untrusted` (default), `Trusted` (opt-in via `--trust-project-config`/`MCPLS_TRUST_PROJECT_CONFIG`) |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|--------------------|
| `$MCPLS_CONFIG` points at a nonexistent path | `Error::ConfigNotFound` |
| Config file larger than `MAX_CONFIG_FILE_BYTES` (8 MiB) | Rejected via `Error::FileSizeLimitExceeded` before the whole file is buffered, including for special files (e.g. `/dev/zero`) that report `metadata().len() == 0` |
| Config file exactly at the 8 MiB boundary | Accepted — the boundary itself is not rejected |
| Config file is not valid UTF-8 | `Error::InvalidConfig` naming the UTF-8 decode failure |
| Project-local `./mcpls.toml` exists, trust untrusted | Skipped entirely, warning logged, `project_config_ignored: true`, falls through to the next tier |
| Two `[[lsp_servers]]` entries share a `language_id` with no explicit `name` on either | Both resolve to the same default `ServerId` — caught later by `ToolRouter::from_configs` ([[mcp/001-mcp-tool-surface-and-routing/spec|spec mcp/001]]), not by `ServerConfig::validate` itself, since two servers legitimately sharing a language with mutually exclusive `heuristics` is valid until a workspace makes both applicable |
| A relative `workspace.roots` entry in an explicitly-named config | Resolved against that config file's own directory, then canonicalized |
| A relative `workspace.roots` entry in the auto-discovered global/user config | Resolved against the process's current working directory instead (not tied to a particular project) |
| A workspace root that does not exist on disk | `Error::InvalidConfig` naming the missing root and the base directory it was resolved against |
| `workspace.roots` entry is an empty string | Rejected explicitly (`Path::is_relative()` is `true` for an empty path and would otherwise silently resolve to the base directory unchanged) |
| Project marker exists only inside `node_modules`/`target`/`.git`/etc. | Not found — these directories are excluded from the recursive walk entirely (not merely deprioritized) |
| Project marker exists at a depth beyond `heuristics_max_depth` | Not found; increasing `heuristics_max_depth` (default 10) finds it |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | `cargo nextest run -E 'package(mcpls-core) and test(config)'` | All existing config-loading, validation, and heuristics unit tests pass |
| SC-002 | Zero-config startup (`ServerConfig::default()`) | Returns 6 built-in servers, ~30 extension mappings, valid per `ServerConfig::validate()` |
| SC-003 | A monorepo integration scenario (Rust root + nested Python/TypeScript subprojects) | All three servers' heuristics correctly detect their respective nested markers |

## 8. Agent Boundaries

### Always (without asking)
- Preserve the `Untrusted`-by-default trust posture for a CWD-discovered `./mcpls.toml` — this is
  a deliberate security decision, not an oversight
- Keep `EXCLUDED_DIRECTORIES` (config/server.rs) in sync with any new well-known
  dependency/build-output directory convention that would otherwise cause false-positive or
  pathologically slow heuristic searches
- Run the config-loading and heuristics test suites after any change to `ServerConfig::validate`,
  `ServerConfig::load_from`, or `ServerHeuristics`

### Ask First
- Adding a new configuration tier or changing tier precedence (FR-001)
- Changing the default trust posture for project-local config (NFR-001) — this is a
  security-relevant default, not a convenience setting

### Never
- Default a CWD-discovered `./mcpls.toml` to `Trusted` — this is the exact supply-chain risk
  US-002 exists to prevent
- Replace the bounded `Read::take` config-file read with a `metadata().len()` pre-check alone —
  this reopens the special-file bypass FR-005/NFR-002 close (#309)

## 9. Open Questions

None — this is a retroactive spec documenting stable, already-shipped, well-tested behavior.

## 10. See Also

- [[constitution]] — project principles (not yet created for this project)
- [[MOC-specs]] — all specifications
- [[bridge/005-expose-document-tracker-limits/spec|spec bridge/005]] — `workspace.max_documents`/`max_file_size`,
  fields on the same `WorkspaceConfig` this spec documents
- [[bridge/001-position-encoding-layer/spec|spec bridge/001]] — the conversion math consuming
  `workspace.position_encodings` once negotiated
- [[lsp/001-lsp-server-lifecycle-and-respawn/spec|spec lsp/001]] — where `position_encodings` is actually
  offered during the `initialize` handshake
- [[mcp/001-mcp-tool-surface-and-routing/spec|spec mcp/001]] — `ToolRouter`, the later-added per-tool
  routing layer built on top of this spec's `LspServerConfig`/`ServerId`
- `crates/mcpls-core/src/config/mod.rs` — `ServerConfig`, `WorkspaceConfig`, `McpConfig`, loading
  and validation
- `crates/mcpls-core/src/config/server.rs` — `LspServerConfig`, `ServerHeuristics`, built-in server
  defaults, `MAX_TIMEOUT_SECONDS`
