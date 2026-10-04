---
aliases:
  - Workspace-supplied code execution
  - Language-server trust model
  - Untrusted workspace hardening
tags:
  - sdd
  - spec
  - research
  - runtime
  - security
  - hardening
created: 2026-10-04
status: implemented
related:
  - "[[constitution]]"
  - "[[config/001-config-discovery-and-heuristics/spec|config-discovery-and-heuristics]]"
  - "[[lsp/007-lsp-child-process-lifetime/spec|lsp-child-process-lifetime]]"
---

# Feature: Workspace-Supplied Code Execution by Default Language-Server Configs

> [!info] Metadata
> **Type**: research / security hardening (vulnerability-class sweep)
> **Priority**: P3
> **Related issues**: #566.

## Decision (#566): documentation plus a best-effort tsserver pin

> [!important] Resolved
> Scope: FR-001 to FR-006, FR-009 implemented. FR-007 and FR-008 (untrusted-workspace mode, #603) are
> deferred to a follow-up issue. Open questions 1 to 3, 7 and 8 are answered below.

- **Docs.** `SECURITY.md` carries the per-server trust table, the `--trust-project-config`
  disclaimer (FR-001, FR-002), the private reporting route (FR-009) and the pin coverage. Only the
  typescript-language-server row was verified live; the other rows state well-known server behavior.
- **Pin.** At startup mcpls resolves the tsserver typescript-language-server would pick as
  "bundled" and sends it as `initializationOptions.tsserver.path`
  (`crates/mcpls-core/src/lsp/tsserver_pin.rs`). Resolution is by node's lookup of the `typescript`
  dependency from the server's package directory, found by following the executable's symlinks. No
  absolute path is shipped (NFR-002, SC-004).
- **Identification** is by the command's file stem, so absolute paths and `.cmd` names are recognized.
  `PATH` is read as the child sees it (config `env` override, else the parent environment).
- **Coverage (SC-002 scope).** `npm -g` style symlink installs (verified with Homebrew's node; the
  Homebrew formula layout and bun were not checked) with a `typescript` package next to the server
  whose `package.json` has a `version`, which the server requires of an install before it honors
  it. Windows `.cmd` shims, script launchers (pnpm, Volta, asdf/mise) and `npx`/`bunx`/`node cli.mjs`
  wrappers that only name the server in `args` are logged as `UnsupportedLauncher` and not pinned.
  A missing or invalid `typescript` package is logged as `NoTypescriptNextToServer`, a missing
  executable as `ServerNotOnPath`. None blocks startup (NFR-005). Follow-up filed for shim support.
- **Inside the workspace.** A server installed inside the workspace is still pinned and a warning is
  logged: skipping would let the server walk the `rootUri` ancestors and pick the workspace tsserver.
- **User options (FR-006).** A user `tsserver.path` wins. User options without `tsserver.path` skip
  the pin with a warning (no merge).
- **Post-init check.** On `$/typescriptVersion` with a pinned `tsserver.path`, a `source` other than
  `user-setting` logs a warning. This catches a stale pin on respawn, because resolution happens once
  at startup.
- **Not covered.** Automatic type acquisition may fetch packages over the network, and tsconfig
  plugins are not loaded from the workspace by a pinned tsserver because `allowLocalPluginLoads` is
  never passed; this holds only while the pin applies. The startup read of the install layout adds no
  LSP requests (NFR-006): the post-init check reads a notification the server sends anyway.
- **Behavior change (NFR-003).** Projects relying on a workspace-pinned TypeScript now get the
  bundled one by default. Opt out by setting `initialization_options.tsserver.path`. Recorded in
  CHANGELOG as breaking.

## 1. Overview

### Problem Statement

mcpls spawns language servers whose own project-loading behavior executes code supplied by the
analyzed workspace. mcpls's default server configs do nothing to prevent or disclose this.

**Verified live in a scratch workspace.** A TypeScript language server resolves its bundled
compiler service (tsserver) in the order user setting, then workspace, then bundled, so a
workspace-supplied compiler script runs inside the server process tree before any tool call. The
scratch workspace was opened with the same `initialize` / `didOpen` sequence mcpls performs (cwd and
`rootUri` set to the workspace). The upstream mitigation is pinning the compiler-service location
through the server's initialization options.

**mcpls adds no mitigation.** `LspServerConfig::typescript()` in
`crates/mcpls-core/src/config/server.rs` sets `initialization_options: None` (via the shared
`builtin` constructor, which does so for every built-in server). mcpls only forwards whatever a
config supplies in the `initialization_options` field (`server.rs:175`).

**The same class applies to other default servers:**

| Server | Workspace-supplied code reachable by the server |
|--------|--------------------------------------------------|
| typescript-language-server | A workspace-supplied tsserver script (verified) |
| rust-analyzer | Cargo build scripts and procedural macros |
| pyright / pylsp | Project Python environment, plugins, config-referenced interpreters |
| gopls | Go environment and toolchain downloads (`go.mod` toolchain directive) |
| clangd | `compile_commands.json` and `.clangd` configuration |

Rows other than typescript-language-server are the finding's attack-class list and are
[NEEDS CLARIFICATION: not individually verified live in this cycle].

**Existing coverage does not address this.** The `./mcpls.toml` trust gate (#229,
`--trust-project-config`, `docs/user-guide/configuration.md` "Trusting a Project-Local Config",
`README.md` near L264) protects against workspace-supplied server *commands and env*. It does not
cover the *language server itself* executing workspace code, and trusting the config is orthogonal
to trusting the workspace. The only analogous trust note in the repository is
`docs/benchmarks.md` "Trust model" (rust-analyzer build scripts and proc macros, package installation),
which is scoped to the benchmark harness, not to users running mcpls.

**Reporting channel gap.** The repository has no `SECURITY.md` (verified: no `SECURITY.md`,
`.github/SECURITY.md` or `docs/SECURITY.md`). Reports of this class therefore have no documented
private disclosure path and are filed as ordinary public issues. See Open Questions.

> [!warning] Scope of the claim
> This behavior is inherent to LSP and identical for every comparable code-intelligence bridge. It is a hardening and
> documentation gap, not an mcpls vulnerability in the narrow sense. mcpls is typically pointed at
> the user's own repositories; the exposure arises when an AI client is pointed at an
> untrusted checkout (a cloned third-party repository, a pull-request branch) and mcpls auto-spawns
> a default server there.

### Goal

A user running mcpls against a workspace can learn, from mcpls's own documentation, that each
default language server may execute code from that workspace, and the default configuration does
not silently rely on workspace-supplied binaries where a reliable, portable alternative exists.

### Out of Scope

- Sandboxing or containerizing language-server processes (OS-level isolation, seccomp, namespaces) — a separate, much larger concern.
- Preventing a language server from executing workspace code in general: build scripts, proc macros, Python environments and toolchain downloads are the functional basis of those servers.
- The `./mcpls.toml` trust gate (#229): unchanged; this spec concerns a threat the gate does not cover.
- Any change to LSP lifetime/process-tree binding (see `lsp/007-lsp-child-process-lifetime`).
- Technical design — to be recorded in a plan after this spec is approved.

## 2. User Stories

### US-001: User understands the trust model before opening an unfamiliar workspace

AS A mcpls user who may point an AI client at third-party repositories
I WANT the documentation to state, per default server, what workspace-supplied code that server may execute
SO THAT I can decide whether to trust a checkout before mcpls spawns a server in it.

**Acceptance criteria:**
```
GIVEN a user reading the mcpls user guide or README
WHEN they look for the security implications of running mcpls against an untrusted checkout
THEN they find, per default language server, the class of workspace code it can execute
  and a statement that the --trust-project-config gate does not cover this
```

### US-002: Default TypeScript setup does not execute the workspace's tsserver

AS A mcpls user with the auto-generated default config
I WANT typescript-language-server not to pick up a workspace-supplied tsserver without my consent
SO THAT merely opening an untrusted checkout does not run its code through the server.

**Acceptance criteria:**
```
GIVEN a workspace with package.json, a .ts file and a workspace-supplied fake tsserver script
  that writes a marker file
WHEN mcpls with the default config spawns typescript-language-server for that workspace
  and the first tool call opens the .ts file
THEN the fake tsserver script is not executed and no marker file is created
```

### US-003: User can explicitly opt in to workspace-supplied tooling

AS A mcpls user working on my own project that legitimately pins a TypeScript version inside the workspace
I WANT a documented, explicit way to restore workspace tsserver resolution
SO THAT hardening the default does not break my project's pinned toolchain.

**Acceptance criteria:**
```
GIVEN the hardened default
WHEN the user supplies the documented explicit override in their config
THEN the server resolves tsserver from the workspace as before
```

### US-004: Operator working on untrusted code can run in a restricted mode

AS A security-conscious operator analyzing an untrusted checkout
I WANT an explicit "untrusted workspace" posture that refuses to spawn servers known to execute workspace code unless I consent
SO THAT I do not depend on per-server knowledge to stay safe.

**Acceptance criteria:**
```
GIVEN the untrusted-workspace mode is enabled
WHEN a tool call would spawn a server classified as executing workspace code
THEN mcpls reports a typed, actionable refusal and spawns nothing
```

This story is conditional on the option decision in Open Questions.

### US-005: Reporter knows how to file a security report

AS A security researcher who finds a vulnerability-class issue in mcpls
I WANT a documented reporting channel
SO THAT I do not have to disclose in a public issue.

**Acceptance criteria:**
```
GIVEN the repository root
WHEN a reporter looks for security policy
THEN a SECURITY.md (or equivalent GitHub security policy) states the supported versions and the private reporting route
```

## 3. Functional Requirements

Options weighed (not mutually exclusive). Priority reflects the P3 research nature: documentation is
the committed minimum, the rest is gated on the open decisions.

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | THE SYSTEM SHALL document, in the user guide and README, the trust model per default server: which workspace-supplied code each of typescript-language-server, rust-analyzer, pyright, gopls and clangd can execute | must |
| FR-002 | WHEN documenting the model THE SYSTEM SHALL state explicitly that `--trust-project-config` governs only workspace-supplied mcpls config (commands, env) and does not make a workspace safe to analyze | must |
| FR-003 | THE SYSTEM SHALL document the configuration the user supplies to pin or override each server's workspace-resolved tooling where an upstream control exists (for typescript-language-server: the initialization option that pins the tsserver location) | must |
| FR-004 | WHEN the default typescript-language-server config is used and a tsserver can be resolved reliably and portably outside the workspace THE SYSTEM SHALL pin it so a workspace-supplied tsserver is not selected | should |
| FR-005 | IF no reliable, portable out-of-workspace tsserver can be resolved THEN THE SYSTEM SHALL fall back to documented behavior (FR-001, FR-003) rather than hard-coding an absolute path | must |
| FR-006 | WHERE the user explicitly overrides `initialization_options` for a server THE SYSTEM SHALL honor the user's value over any built-in hardening default | must |
| FR-007 | WHERE an "untrusted workspace" mode exists THE SYSTEM SHALL represent the workspace trust level as a closed typed value, not a boolean flag or free-form string | should |
| FR-008 | WHERE an "untrusted workspace" mode exists AND a server is classified as executing workspace code WHEN a tool call would spawn it THE SYSTEM SHALL refuse with a typed error that names the server and the consent mechanism | should |
| FR-009 | THE SYSTEM SHALL publish a security policy (`SECURITY.md`) describing the private reporting route | should |
| FR-010 | THE SYSTEM SHALL record, for each default server, whether built-in hardening exists, so the documentation and any classification stay consistent | could |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Type safety | Per-server hardening and any trust level SHALL be expressed with typed values (enums for server identity and trust level, typed initialization options for the pinned path) per [[constitution]]; no stringly-typed or untyped-map plumbing for the hardening default beyond the existing `initialization_options` boundary |
| NFR-002 | Portability | No hard-coded absolute filesystem path SHALL be shipped as a default; any resolved path SHALL be derived at runtime and valid on Linux, macOS and Windows |
| NFR-003 | Backward compatibility | Pre-v1.0.0 compatibility is not a constraint, but a behavior change to the default typescript config SHALL be recorded in `CHANGELOG.md` as a breaking change if it can alter which tsserver a project uses |
| NFR-004 | Honesty of claims | Documentation SHALL NOT claim mcpls makes an untrusted workspace safe; it SHALL state that server-side execution of workspace code is inherent to LSP |
| NFR-005 | Graceful degradation | Hardening failure (for example tsserver not resolvable) SHALL NOT prevent the server from starting; it SHALL degrade to documented upstream behavior, consistent with the multi-server graceful-degradation principle |
| NFR-006 | No extra round-trips | Hardening SHALL NOT add LSP requests to existing tool flows |

## 5. Data Model

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| Server execution profile | Per built-in server, the class of workspace code it may execute and whether a built-in pin exists | server identity (existing built-in server enum), executed-code classes, hardening available |
| Workspace trust level | Closed set describing the operator's posture toward the analyzed workspace (only if the mode is adopted) | trusted, untrusted |
| Tool resolution pin | The resolved out-of-workspace tool location passed through the existing initialization options | resolved path, provenance (resolved at runtime vs user-supplied) |

No persistent storage is introduced.

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Bundled tsserver cannot be located (typescript-language-server installed in an unusual layout or launched through a one-off runner) | Fall back to upstream resolution and rely on documentation; do not fail to start (FR-005, NFR-005) |
| Project legitimately depends on a specific workspace-local TypeScript version | Pinning the bundled tsserver changes language-service behavior for that project; user can opt out per FR-006 and US-003. Must be documented |
| User already supplies `initialization_options` for the typescript server | User value wins; mcpls does not merge or overwrite (FR-006) |
| Workspace-supplied `./mcpls.toml` sets `initialization_options` to repoint tsserver | Governed by the existing config trust gate; the pin does not defend a trusted config |
| Symlinked or relocated workspace-local TypeScript installation pointing outside the workspace | Resolution is by the server, not mcpls; documented as outside mcpls control |
| Server other than TypeScript (rust-analyzer build scripts, proc macros) | No upstream pin exists that avoids the behavior without breaking the server; documentation only (FR-001) |
| Untrusted-workspace mode enabled and a configured server is not classified | [NEEDS CLARIFICATION: treat unclassified (including user-defined) servers as executing workspace code, or as safe] |
| Windows path and extension differences for tsserver | Pin must be resolved with platform-correct paths (NFR-002) |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Default servers with documented workspace-code-execution class | 5 of 5 (typescript-language-server, rust-analyzer, pyright, gopls, clangd) |
| SC-002 | Live repro (US-002) with default config | No marker file created |
| SC-003 | Live repro (US-002) after the documented opt-in (US-003) | Marker file created, confirming the override is honored |
| SC-004 | Default config containing a hard-coded absolute tool path | 0 |
| SC-005 | Existence of a documented private reporting route | Present in repository root |

## 8. Agent Boundaries

### Always (without asking)
- Reproduce the finding against a scratch workspace outside the repository before changing behavior, and keep the repro as a regression case under `.local/testing/`.
- Keep user-supplied `initialization_options` authoritative over built-in defaults (FR-006).
- Model any trust level and server classification with enums, per [[constitution]].

### Ask First
- Choosing between documentation-only, per-server pinning, and an untrusted-workspace mode (Open Questions).
- Changing the default typescript server behavior in a way that alters which tsserver existing projects use.
- Adding a dependency to locate the bundled tsserver (constitution VII: justify new dependencies).
- Adding a CLI flag or environment variable for workspace trust.

### Never
- Hard-code an absolute filesystem path as a default (NFR-002).
- Claim in documentation or errors that mcpls sandboxes or makes an untrusted workspace safe (NFR-004).
- Weaken or reinterpret the `--trust-project-config` gate (#229) as a workspace-trust control.
- Publish a working exploit for this class beyond the minimal generic reproduction recorded in this spec.

## 9. Open Questions

> [!question] Decisions needed before a plan
> - [NEEDS CLARIFICATION: scope decision — documentation only (FR-001 to FR-003, FR-009), documentation plus typescript pinning (FR-004, FR-005), or additionally an untrusted-workspace mode (FR-007, FR-008)? Recommended default for P3: documentation plus a best-effort typescript pin.]
> - [NEEDS CLARIFICATION: can the bundled tsserver be resolved reliably and portably (relative to the resolved typescript-language-server install, across global package-manager installs, one-off runners and Windows), or is any hard-coded path inherently non-portable? If not reliably resolvable, FR-004 drops to documentation only.]
> - [NEEDS CLARIFICATION: is the behavior change acceptable for projects relying on a workspace-pinned TypeScript version, given NFR-003 and the opt-out in US-003?]
> - [NEEDS CLARIFICATION: how should an untrusted-workspace mode be activated (CLI flag, environment variable, config key) and does it relate to the existing global `--trust-project-config` flag, which is not scoped per project?]
> - [NEEDS CLARIFICATION: classification of user-defined and unclassified servers in untrusted mode.]
> - [NEEDS CLARIFICATION: which of the non-TypeScript rows were verified live; rust-analyzer, pyright, gopls and clangd claims come from the finding's class list and need per-server confirmation before they are published as fact.]
> - [NEEDS CLARIFICATION: SECURITY.md contents: supported versions, private contact or GitHub private vulnerability reporting enabled on the repository, and disclosure timeline.]
> - [NEEDS CLARIFICATION: issue number to record in the Metadata callout once filed.]

## 10. See Also

- [[constitution]] — project principles (type safety, security, simplicity)
- [[MOC-specs]] — all specifications
- [[config/001-config-discovery-and-heuristics/spec|config-discovery-and-heuristics]] — built-in server configs and the project-config trust gate (#229)
- [[lsp/007-lsp-child-process-lifetime/spec|lsp-child-process-lifetime]] — server process-tree binding to mcpls lifetime
- `docs/user-guide/configuration.md` "Trusting a Project-Local Config" and `README.md` near L264 — existing trust note covering workspace-supplied commands and env only
- `docs/benchmarks.md` "Trust model" — only existing note on build scripts and proc macros, scoped to the benchmark harness
- `crates/mcpls-core/src/config/server.rs` — `LspServerConfig::typescript()`, `builtin` constructor, `initialization_options` field (`:175`)
