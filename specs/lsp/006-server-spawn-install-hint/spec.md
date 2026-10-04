---
aliases:
  - LSP Server Spawn Install Hint
  - Missing Binary Install Guidance
tags:
  - sdd
  - spec
  - research
  - lsp-bridge
created: 2026-09-21
status: implemented
related:
  - "[[constitution]]"
---

# Feature: Install Guidance on LSP Server Spawn Failure

> [!info] Metadata
> **Author**: Andrei G.
> **Source**: continuous-improvement cycle 035, competitive-parity research finding (P3)

## 1. Overview

### Problem Statement

mcpls auto-discovers LSP servers via project-marker heuristics
(`crates/mcpls-core/src/config/`, e.g. `Cargo.toml` → `rust-analyzer`,
`package.json` → `typescript-language-server`) and auto-generates a default
config with 6 language → server mappings (the `BuiltinServer` table in
`crates/mcpls-core/src/config/server.rs`) on first run. But mcpls never checks whether the resulting
server binary is actually present on `PATH` before trying to spawn it, and it
does not maintain any "how to install this server" hint table.

When a configured or discovered LSP server binary is missing,
`LspServer::spawn` (`crates/mcpls-core/src/lsp/lifecycle.rs:398`) calls
`Command::spawn()` and on failure wraps the raw `std::io::Error` into
`Error::ServerSpawnFailed { command, source }`
(`crates/mcpls-core/src/error.rs:181-189`). That variant's `Display`
implementation renders only the command name and the raw OS error text — for
example, on Unix, something like:

```
failed to spawn LSP server 'rust-analyzer': No such file or directory (os error 2)
```

There is:
- No install command surfaced (e.g. `rustup component add rust-analyzer` for
  `rust-analyzer`, or `npm install -g typescript-language-server` for
  `typescript-language-server`).
- No distinction between "binary not found" (`ENOENT` /
  `io::ErrorKind::NotFound`) and other spawn failures (permission denied,
  exec-format errors, etc.) that would call for different guidance.
- No pre-flight `PATH` check before attempting the spawn, so the failure is
  only discovered reactively, at the moment an MCP tool call needs that
  server.

This matters because mcpls's core value proposition is zero-friction
auto-discovery — the whole point of the project-marker heuristics and the
auto-generated default config is that a user shouldn't have to hand-write LSP
server configuration. A bare OS errno at the point of first use undercuts
that promise: the user has done everything right (opened a project mcpls
correctly recognized), but the failure message gives them no path forward
other than researching, outside of mcpls, which package manager or installer
provides `rust-analyzer` (or any of the other 5 builtin servers).

### Goal

When a configured or auto-discovered LSP server binary cannot be found or
spawned, the error (or a diagnostic surface) that reaches the user includes
an actionable install hint for well-known servers — the same set already
covered by the builtin server table in `config/server.rs` — and
distinguishes "binary not on PATH" from other spawn failures where the
distinction changes the guidance given.

### Out of Scope

- Automatically installing the missing server on the user's behalf (this is
  about surfacing guidance, not performing installation).
- A general-purpose "doctor" / environment-diagnostics command, unless the
  plan phase determines that is the best surface for this guidance
  ([NEEDS CLARIFICATION: see Open Questions] — this spec covers the
  capability, not necessarily a specific new CLI subcommand).
- Install hints for LSP servers not already in mcpls's builtin/default
  server table — user-configured custom servers with no known install
  command simply keep today's raw-error behavior.
- Verifying that an installed binary is actually a *working*, compatible LSP
  server (version checks, capability negotiation) — this is strictly about
  the binary-not-spawnable case.
- Changing `LspServer::spawn`'s respawn/retry behavior for servers that
  spawn successfully but crash later (already covered by
  [[lsp/001-lsp-server-lifecycle-and-respawn/spec|001-lsp-server-lifecycle-and-respawn]]).

## 2. User Stories

### US-001: Actionable error on missing LSP server binary

AS A developer opening a project mcpls auto-configured
I WANT the error I see when an LSP server binary is missing to tell me how to
install it
SO THAT I don't have to leave mcpls to figure out which package manager or
installer provides that language server.

**Acceptance criteria:**
```
GIVEN mcpls has discovered or been configured to spawn a well-known LSP
  server (e.g. rust-analyzer) whose binary is not on PATH
WHEN LspServer::spawn attempts to spawn that server and the OS reports the
  binary was not found (ENOENT / io::ErrorKind::NotFound)
THEN the error surfaced to the user includes both the original spawn-failure
  detail and an install command or instruction specific to that server
AND if the server is not one mcpls has a known install hint for, the error
  falls back to today's behavior (command name + raw OS error) without
  regressing
```

### US-002: Distinguish "not found" from other spawn failures

AS A developer whose LSP server binary exists but fails to spawn for another
reason (e.g. permission denied, not executable)
I WANT the error message to reflect that the binary exists but couldn't be
launched, rather than suggesting an install
SO THAT I troubleshoot the actual problem (permissions, corrupt binary)
instead of reinstalling a server that is already present.

**Acceptance criteria:**
```
GIVEN a configured LSP server binary exists on PATH but Command::spawn()
  fails for a reason other than "not found" (e.g. io::ErrorKind::PermissionDenied)
WHEN LspServer::spawn reports the failure
THEN the error does not suggest an install command
AND the error text or structure indicates the failure was not a
  "binary missing" condition
```

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | THE SYSTEM SHALL maintain a table mapping well-known LSP server command names (the same set covered by the builtin server table in `config/server.rs`) to a human-readable install hint (command or instruction) | must |
| FR-002 | WHEN `LspServer::spawn` receives a `std::io::Error` from `Command::spawn()` whose `kind()` is `io::ErrorKind::NotFound` THE SYSTEM SHALL classify the failure as "binary not on PATH" | must |
| FR-003 | WHEN a spawn failure is classified as "binary not on PATH" (FR-002) AND the failing command name matches an entry in the install-hint table (FR-001) THE SYSTEM SHALL include that hint in the error surfaced to the caller | must |
| FR-004 | WHEN a spawn failure is classified as "binary not on PATH" (FR-002) AND the failing command name does NOT match any entry in the install-hint table THE SYSTEM SHALL surface `Error::ServerNotFound` with the command name, the raw OS error, and PATH guidance, with no install hint fabricated | must |
| FR-005 | WHEN a spawn failure's `io::ErrorKind` is anything other than `NotFound` (e.g. `PermissionDenied`) THE SYSTEM SHALL NOT include an install hint in the surfaced error | must |
| FR-006 | THE SYSTEM SHALL surface the install hint (when present) through the `Display` text of the new `Error::ServerNotFound` variant (see Open Questions for delivery) | must |
| FR-007 | THE SYSTEM SHALL cover, at minimum, install hints for every server already present in the builtin default-config table in `config/server.rs` (6 entries, `BuiltinServer`) | should |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Maintainability | The install-hint table must be a single source of truth co-located with (or clearly cross-referenced to) the existing builtin server table in `config/server.rs`, so the two do not drift apart as servers are added or removed |
| NFR-002 | Portability | Install hints that differ by OS or package manager (e.g. `rust-analyzer` via `rustup component add` vs. a system package manager) must not assume a single platform; where a hint is genuinely platform-specific, the platform must be indicated in the hint text |
| NFR-003 | Reliability | Adding install-hint logic must not change behavior for the success path (server spawns fine) or for spawn failures of unlisted/custom servers — no regression to `[[lsp/001-lsp-server-lifecycle-and-respawn/spec|001-lsp-server-lifecycle-and-respawn]]`'s existing respawn/backoff behavior |

## 5. Data Model

No new persistent data entities. This feature adds a static, in-code lookup
table and enriches an existing error path.

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| Install hint entry | Maps a known LSP server command name to install guidance | `command` (e.g. `"rust-analyzer"`), `hint` (human-readable instruction or shell command), optionally a platform qualifier |
| Spawn failure classification | Distinguishes why `Command::spawn()` failed | `io::ErrorKind` (e.g. `NotFound`, `PermissionDenied`), original `std::io::Error` |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Configured command is a well-known server but binary is missing | Error includes install hint (FR-003) |
| Configured command is a custom/unlisted server and binary is missing | Error falls back to existing raw-error behavior, no fabricated hint (FR-004) |
| Configured command exists on PATH but is not executable (permission denied) | Error does not suggest reinstalling; indicates a permissions problem instead (FR-005) |
| Configured command name collides with an install-hint table entry but resolves to a different, user-supplied binary of the same name that happens to be missing | Hint is still shown (name-based match only — this feature does not attempt to disambiguate intent) — flagged as an acceptable false-positive in [[#9. Open Questions]] |
| Multiple LSP servers configured, only one binary missing | Only the failing server's spawn attempt gets the enriched error; other servers continue to spawn/operate normally (existing graceful-degradation behavior, unaffected) |
| Install-hint table entry becomes stale (e.g. install command changes upstream) | Out of scope for this spec — treated as ordinary table maintenance, not a runtime failure mode |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Spawn failure for a well-known missing server includes a non-empty install hint distinct from the raw OS error text | 100% of builtin-table servers covered (FR-007) |
| SC-002 | Spawn failure for a non-`NotFound` `io::ErrorKind` never includes an install hint | 0 false-positive hints in test coverage of `PermissionDenied`-style failures |
| SC-003 | Spawn failure for an unlisted/custom command carries the raw error plus PATH guidance, but no install hint | Covered by `test_server_not_found_bare_unknown_has_no_install_hint` |

## 8. Agent Boundaries

### Always (without asking)
- Run the full check suite before considering this done: `cargo +nightly fmt --check`, `cargo clippy --all-targets --all-features --workspace -- -D warnings`, `cargo nextest run --workspace --all-features --lib --bins`, rustdoc gate
- Keep the install-hint table's server list in sync with the builtin table in `config/server.rs` when either changes
- Keep `Error::ServerSpawnFailed` for non-`NotFound` spawn errors (FR-005)

### Ask First
- Adding a new CLI subcommand (e.g. a "doctor" command) as the delivery surface for hints, if the plan phase concludes that's preferable to error-message enrichment alone

### Never
- Fabricate an install hint for a server not verified to actually be installable via the suggested command
- Attempt to auto-install a missing binary without explicit user action (out of scope, see [[#Out of Scope]])
- Change respawn/backoff behavior for servers that spawn successfully but later crash

## 9. Open Questions

- [RESOLVED] Delivery: a new `Error::ServerNotFound` variant (typed ENOENT) whose `Display` carries the guidance. It reaches stderr logs, the respawn-path error, and (#527) tool calls routed to a server that failed its initial spawn: they return `Error::ServerFailedToStart`, whose `Display` carries the same guidance, and workspace-wide tools return `Error::AllServersFailedToInit` listing every failure. A server that exits before answering `initialize` yields `Error::ServerExitedDuringInit`, with a rustup-proxy hint for rust-analyzer. Capturing the server's stderr into these errors is a follow-up.
- [RESOLVED] Hints are static, one per `BuiltinServer`, with no OS detection; the Windows `.cmd` note is added only for npm-distributed builtins.
- [NEEDS CLARIFICATION: Is a pre-flight `PATH` check before spawn (proactively warning at config-load or server-registration time, rather than only reactively on spawn failure) in scope for this spec, or is that a follow-up enhancement? The finding's reproduction is reactive (spawn-time) only.]
- [NEEDS CLARIFICATION: Should this feature also cover a `doctor`/diagnostic-style MCP tool or CLI subcommand that reports install status for all configured servers up front, as `squiggles init` does (see below), or is reactive error enrichment sufficient for a first iteration?]
- [NEEDS CLARIFICATION: The competitive signal (`carldaws/squiggles`) is currently a single-reference-project signal, not yet corroborated by a second reference project, per the original P3 finding — confirm this remains P3 rather than being reprioritized once/if a second reference project ships similar behavior.]

## 10. See Also

- [[constitution]] — project principles
- [[MOC-specs]] — all specifications
- [[lsp/001-lsp-server-lifecycle-and-respawn/spec|lsp/001-lsp-server-lifecycle-and-respawn]] — existing spawn/respawn lifecycle this feature extends, not replaces
- [[config/001-config-discovery-and-heuristics/spec|config/001-config-discovery-and-heuristics]] — the project-marker heuristics and auto-generated default config that produce the 6 language → server mappings referenced here
- `crates/mcpls-core/src/lsp/lifecycle.rs:398` — `LspServer::spawn`, where `Command::spawn()` is called and `Error::ServerSpawnFailed` is constructed
- `crates/mcpls-core/src/error.rs:181-189` — `Error::ServerSpawnFailed { command, source }` variant and its `Display` impl
- `crates/mcpls-core/src/config/server.rs` — `BuiltinServer`, the single source of the 6 builtin commands and their install hints
- `crates/mcpls-core/src/config/language.rs` — related config-module code (React-variant language-id mapping); cited in the originating finding as part of the `config/` marker/mapping surface this feature sits alongside
- [`carldaws/squiggles`](https://github.com/carldaws/squiggles) — reference project; release 1.1.0 (2026-09-21) added a `squiggles init` command that detects project languages via marker heuristics, writes a config, and prints the install command for every LSP server binary its presets reference; commits `79a1080` and `b83f056`
