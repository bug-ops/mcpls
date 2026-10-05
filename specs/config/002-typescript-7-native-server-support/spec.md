---
aliases:
  - TypeScript 7 native server support
  - typescript-language-server with TypeScript 7
  - tsc --lsp default TypeScript server
tags:
  - sdd
  - spec
  - bug
  - config
  - typescript
  - compatibility
created: 2026-10-05
status: draft
related:
  - "[[constitution]]"
  - "[[config/001-config-discovery-and-heuristics/spec|config-discovery-and-heuristics]]"
  - "[[lsp/006-server-spawn-install-hint/spec|server-spawn-install-hint]]"
  - "[[runtime/003-workspace-supplied-code-execution/spec|workspace-supplied-code-execution]]"
---

# Feature: Working Default TypeScript Server When `typescript` Is Version 7

> [!info] Metadata
> **Author**: Andrei G.
> **Type**: bug (compatibility with an upstream ecosystem change)
> **Priority**: P1
> **Source**: continuous-improvement cycle, live-test finding reproduced 2026-10-05 against mcpls 0.6.0 (HEAD 517cb53)
> **Issue**: #615

> [!abstract]
> The documented install command for mcpls's built-in TypeScript server now installs a
> `typescript` package (7.x, the native port) that `typescript-language-server` cannot use. A
> fresh install that follows mcpls's own instructions yields a TypeScript server that fails to
> initialize, with an upstream message that does not mention TypeScript 7. This spec covers
> correcting the guidance, detecting the situation, telling the user exactly what to do, and
> optionally selecting the native TypeScript server automatically under the workspace-code-execution
> trust model.

## 1. Overview

### Problem Statement

The built-in TypeScript server is `typescript-language-server`
(`BuiltinServer::TypescriptLanguageServer` in `crates/mcpls-core/src/config/server.rs`). Its install
hint (`BuiltinServer::install_hint`) and `README.md` ("TypeScript" install block) both give
`npm install -g typescript-language-server typescript`.

On npm the `typescript` `latest` dist-tag is now 7.0.2 (published 2026-07-08; 7.0.1-rc on
2026-06-18). TypeScript 7 is the native (Go) port:

- its npm package no longer ships `lib/tsserver.js`;
- it exposes `bin/tsc` and a built-in language server started with `tsc --lsp --stdio`.

`typescript-language-server` 6.0.1 (latest, 2026-09-24) bundles no TypeScript. It relies on a
`typescript` package being installed and requires tsserver. With only TypeScript 7 available it
cannot initialize.

**Consequence.** A fresh install following mcpls's documented command produces a non-functional
TypeScript language server.

**Interaction with the tsserver pin (#609, `crates/mcpls-core/src/lsp/tsserver_pin.rs`).** The pin
looks for `node_modules/typescript/lib/tsserver.js` next to the server package. For TypeScript 7 it
finds none, leaves the options unpinned and only logs a warning. The log text
(`no valid typescript package is installed next to typescript-language-server`) is
misleading, because a valid TypeScript 7 package is installed. The server then falls back to
workspace lookup and fails.

**Reproduced live (2026-10-05, mcpls 0.6.0, HEAD 517cb53):**

1. `npm i typescript@7.0.2 typescript-language-server@6.0.1` in a scratch directory containing
   `tsconfig.json` and `a.ts`.
2. `ls node_modules/typescript/lib` shows only `getExePath.*`, `tsc.js` and `version.*`; there is
   no `tsserver.js`.
3. Run mcpls with `[[lsp_servers]] language_id = "typescript"`,
   `command = ".../node_modules/.bin/typescript-language-server"`, `args = ["--stdio"]`, and call
   `get_hover` on `a.ts`.
4. Result: JSON-RPC error `-32603`:
   `LSP server 'typescript' ... failed to start: ... Request initialize failed with message: Could not find a valid TypeScript installation. Please ensure that the "typescript" dependency is installed in the workspace or that a valid tsserver.path is specified. Exiting.; restart mcpls after fixing it (startup failures are not retried)`.
5. Control: the same workspace with `command = ".../node_modules/.bin/tsc"`,
   `args = ["--lsp", "--stdio"]` returns a correct hover (`function f(a: string): number`)
   through mcpls. The native server therefore works with the existing client. mcpls already carries
   native-server compatibility fixes (closed #100 and #403, and a `tsgo --lsp --stdio` config test).

**Observed, not yet verified against mcpls.** The native TypeScript 7 server reports diagnostics
only through pull (`textDocument/diagnostic`). mcpls supports pull diagnostics
(`bridge/translator/diagnostics.rs`), but the end-to-end behavior against the native server needs a
live check ([NEEDS CLARIFICATION: see FR-008]). Neither TypeScript server supports type hierarchy.
At least one independent MCP LSP bridge already handles the split: when the workspace's own
`typescript` package is version 7 or later it starts that package's `tsc --lsp --stdio`, otherwise
`typescript-language-server`, and a user-defined TypeScript server always takes precedence.

**Trust-model constraint.** Selecting a native `tsc` found in a workspace's `node_modules` means
executing a workspace-supplied binary. [[runtime/003-workspace-supplied-code-execution/spec|runtime/003]]
(#566, `SECURITY.md`, the tsserver pin) exists to avoid exactly that for the default
configuration. Any automatic selection must preserve it.

### Goal

Following mcpls's documented install path yields a working TypeScript server whichever major version
of `typescript` npm installs. When `typescript-language-server` cannot work because only
TypeScript 7 is available, the user is told exactly what to do. Where it can be done without
widening the workspace-code-execution surface, mcpls selects the native server itself.

### Out of Scope

- Bundling, vendoring or installing TypeScript, `typescript-language-server` or the native server on the user's behalf (guidance only, consistent with [[lsp/006-server-spawn-install-hint/spec|lsp/006]]).
- Supporting `typescript-language-server` against TypeScript 7 (an upstream concern).
- Type hierarchy for TypeScript: neither server supports it; the existing unsupported-capability behavior stands.
- Untrusted-workspace mode (#603, deferred by [[runtime/003-workspace-supplied-code-execution/spec|runtime/003]]).
- Pin support for Windows `.cmd` shims, script launchers and `npx`/`bunx`/`node` wrappers (#604).
- Changes to non-TypeScript built-in servers.
- Technical design: recorded in a plan after this spec is approved.

## 2. User Stories

### US-001: Documented install command yields a working server

AS A developer setting up mcpls for a TypeScript project from the README
I WANT the documented install command to produce a TypeScript server that initializes
SO THAT the first tool call on a `.ts` file succeeds.

**Acceptance criteria:**
```
GIVEN a clean machine and the install command documented by mcpls for the TypeScript server
WHEN the developer runs it and then calls get_hover on a TypeScript file in a project with a tsconfig.json
THEN the call returns the hover result and no initialize failure occurs
```

### US-002: Clear diagnosis when only TypeScript 7 is available

AS A developer whose environment already has TypeScript 7 (global install or workspace dependency)
I WANT mcpls to tell me that `typescript-language-server` cannot use TypeScript 7 and what to do about it
SO THAT I do not have to decode an opaque upstream error.

**Acceptance criteria:**
```
GIVEN typescript-language-server is configured and the only reachable typescript package is version 7 or later
WHEN the server fails to initialize
THEN the error returned to the caller names TypeScript 7 as the cause
AND lists the supported remedies (install a JavaScript-based TypeScript next to the server, or configure the native server)
AND does not interpolate any text supplied by the server beyond the existing sanitized stderr excerpt
```

### US-003: Native server selected automatically when safe

AS A developer with the auto-generated default config and TypeScript 7 installed outside my workspace
I WANT mcpls to start the native TypeScript server for me
SO THAT TypeScript works without writing configuration.

**Acceptance criteria:**
```
GIVEN no user-defined TypeScript server, a TypeScript 7 install outside the workspace, and a TypeScript workspace
WHEN a tool call needs the TypeScript server
THEN mcpls starts `tsc --lsp --stdio` from that out-of-workspace install
AND get_hover on a .ts file returns a correct result
```

### US-004: Workspace-supplied native binary is never run without consent

AS A security-conscious operator pointing an AI client at a third-party checkout
I WANT mcpls never to start a `tsc` from that checkout's `node_modules` on its own initiative
SO THAT the trust model of [[runtime/003-workspace-supplied-code-execution/spec|runtime/003]] is not weakened by this feature.

**Acceptance criteria:**
```
GIVEN a workspace whose node_modules/.bin/tsc is a marker-writing script, with default config and no out-of-workspace TypeScript 7
WHEN a tool call needs the TypeScript server
THEN the workspace tsc is not executed and no marker file is created
```

### US-005: User override always wins

AS A developer who configured my own TypeScript server entry
I WANT mcpls never to replace or reinterpret it
SO THAT my explicit choice (including a workspace-local `tsc`) is honored.

**Acceptance criteria:**
```
GIVEN a [[lsp_servers]] entry for language_id "typescript" with an explicit command
WHEN mcpls starts
THEN that command and args are spawned unchanged, whatever TypeScript version is installed
```

### US-006: Diagnostics work against the native server

AS A developer using the native TypeScript server
I WANT `get_diagnostics` to report my type errors
SO THAT the verify loop works as it does with other servers.

**Acceptance criteria:**
```
GIVEN the native TypeScript server and a .ts file containing a type error
WHEN get_diagnostics is called for that file
THEN the response contains the error with correct 1-based position and message
```

## 3. Functional Requirements

Scoping: FR-001 to FR-006 and FR-008 to FR-010 correct the defect and are the committed minimum.
FR-007 and FR-011 to FR-013 are the automatic-selection extension, gated on the open decisions in
section 9.

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | THE SYSTEM SHALL ship an install hint for the built-in TypeScript server (`BuiltinServer::install_hint`) and a README install block that, when followed, yield a `typescript-language-server` that initializes, and SHALL state which `typescript` major versions that server supports | must |
| FR-002 | THE SYSTEM SHALL keep `README.md`, `docs/user-guide/getting-started.md`, `docs/user-guide/configuration.md` and the `BuiltinServer` install hint consistent with each other on the TypeScript install command and the supported `typescript` majors | must |
| FR-003 | WHEN resolving the `typescript` package next to `typescript-language-server` and that package is major version 7 or later (or lacks `lib/tsserver.js` while carrying a `package.json` `version`) THE SYSTEM SHALL classify the situation with a dedicated typed reason, distinct from "no valid typescript package installed" | must |
| FR-004 | WHEN the situation of FR-003 is detected THE SYSTEM SHALL log a warning that names TypeScript 7 and the remedy, instead of the current misleading "no valid typescript package" text | must |
| FR-005 | WHEN `typescript-language-server` fails during initialization AND the situation of FR-003 was detected for that server THE SYSTEM SHALL add static guidance to the surfaced error naming TypeScript 7 as the likely cause and listing the remedies: install a JavaScript-based `typescript` next to the server, or configure the native server explicitly | must |
| FR-006 | THE SYSTEM SHALL document, with a copy-pasteable example, how to configure the native TypeScript server explicitly (`command` resolving to the TypeScript 7 `tsc`, `args = ["--lsp", "--stdio"]`) for `language_id = "typescript"`, including the Windows shim name and that the workspace-local binary is workspace-supplied code | must |
| FR-007 | WHERE automatic native-server selection exists AND no user-defined TypeScript server entry is present AND TypeScript 7 is resolvable from a location outside the workspace THE SYSTEM SHALL start `tsc --lsp --stdio` from that install instead of `typescript-language-server` | should |
| FR-008 | THE SYSTEM SHALL verify live that `get_diagnostics` against the native TypeScript server returns the file's errors through the pull path (`textDocument/diagnostic`), including servers that advertise `diagnosticProvider` without push diagnostics, and SHALL record the result in the testing playbooks | must |
| FR-009 | THE SYSTEM SHALL verify live that the native server accepts mcpls's `initialize` parameters, the workspace-configuration push (see [[lsp/010-workspace-configuration-push/spec|lsp/010]]) and shutdown, and that no `tsserver.path` pin is sent to a server that is not `typescript-language-server` | must |
| FR-010 | WHEN the native TypeScript server is configured THE SYSTEM SHALL answer tools it does not support (type hierarchy) with the existing typed unsupported-capability result, not a transport error | must |
| FR-011 | WHERE automatic selection exists THE SYSTEM SHALL NOT select a `tsc` located inside any workspace root (including through symlinks, per the shared containment predicate of [[bridge/010-workspace-containment-single-predicate/spec|bridge/010]]) without explicit user consent expressed in the user's own configuration | must |
| FR-012 | WHERE automatic selection exists THE SYSTEM SHALL log, at info level, which TypeScript server flavor was chosen and why, and the resolved path of the executable | should |
| FR-013 | WHERE automatic selection exists AND selection fails (no out-of-workspace TypeScript 7, executable not runnable) THE SYSTEM SHALL fall back to `typescript-language-server` and the guidance of FR-005, never abort startup of other servers | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Type safety | The TypeScript server flavor (JavaScript tsserver-based vs native) and the detected `typescript` package state SHALL be closed typed values (enums), not strings or booleans; the major version SHALL be parsed into a typed value, per [[constitution]] |
| NFR-002 | Security | Automatic selection SHALL NOT widen the set of workspace-supplied executables mcpls starts by default compared with [[runtime/003-workspace-supplied-code-execution/spec|runtime/003]]; `SECURITY.md` SHALL describe the native server's workspace-code exposure per its verified behavior |
| NFR-003 | Graceful degradation | Detection and selection failures SHALL NOT prevent mcpls or other servers from starting; they degrade to documented behavior |
| NFR-004 | Performance | Detection SHALL read only the filesystem layout already read by the tsserver pin and add no LSP requests and no process spawns before the server is started |
| NFR-005 | Portability | Detection and selection SHALL be valid on Linux, macOS and Windows; platform-specific executable names (shims) SHALL be handled or reported explicitly, never assumed |
| NFR-006 | Error hygiene | Added error text SHALL be static mcpls text; no value from the server or workspace is interpolated beyond the sanitized excerpt rules of [[runtime/004-server-text-hygiene/spec|runtime/004]] |
| NFR-007 | Honesty of claims | Documentation SHALL NOT claim a pinned or selected native server makes an untrusted workspace safe |
| NFR-008 | Backward compatibility | Pre-v1.0.0 compatibility is not a constraint; automatic selection changes which server the default config starts and SHALL be recorded in `CHANGELOG.md` as a breaking change |
| NFR-009 | Maintainability | The TypeScript install guidance SHALL have one source of truth (`BuiltinServer`), with docs cross-checked against it by a test or a documented checklist |

## 5. Data Model

No persistent data. In-memory typed values only.

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| TypeScript package state | What was found next to the server | installed major (typed), has `lib/tsserver.js`, has native `tsc` entry point, path |
| Unresolved reason (extended) | Why no tsserver could be pinned | existing variants plus a native-TypeScript variant |
| TypeScript server flavor | Which server mcpls starts for `language_id = "typescript"` | `JavaScriptTsserver`, `Native` |
| Selection provenance | Why a flavor was chosen | user-configured, auto-selected, fallback |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Global `typescript` 7 next to `typescript-language-server`, no workspace TypeScript | Detected (FR-003); error and warning name TypeScript 7 (FR-004, FR-005); with automatic selection the native server starts (FR-007) |
| Global `typescript` 7, workspace has its own `typescript` 5 with `tsserver.js` | Pin is not applied; the server selects the workspace tsserver and starts, which is the workspace-code exposure the pin normally prevents. The warning must say so; automatic selection (if adopted) prefers the out-of-workspace native server |
| Workspace has `typescript` 7 only, no global TypeScript | `typescript-language-server` fails; guidance applies. The workspace-local `tsc` is not auto-started (FR-011); the user may configure it explicitly (FR-006, US-005) |
| `typescript` package present but `package.json` has no `version` | Existing behavior: pin skipped, reason "no valid typescript package" |
| `typescript` 7 found in an ancestor `node_modules` after a nearer package without `tsserver.js` | Resolution follows the same node lookup order as the pin; the nearest valid install decides |
| User-defined TypeScript server entry with `tsc --lsp --stdio` | Honored unchanged (US-005); no pin generated since the command is not `typescript-language-server`; `initialization_options` forwarded as given |
| User-defined entry for `typescript-language-server` with TypeScript 7 only | Not replaced; guidance added to its failure (FR-005) |
| Native server installed through a launcher (pnpm, Volta, asdf, mise, `npx`) | Not auto-detected; reported as unsupported launcher; explicit config still works |
| Native server starts but advertises no diagnostics capability | `get_diagnostics` degrades per existing diagnostics behavior; documented by FR-008 outcome |
| Native executable found but not runnable (permissions, wrong platform package) | Fallback per FR-013; spawn failure surfaces through the existing typed errors of [[lsp/006-server-spawn-install-hint/spec|lsp/006]] |
| Windows | `tsc` is a shim (`tsc.cmd`); resolution handles or explicitly reports it (NFR-005) |
| Both `.ts` and `.js` files in project | One native server serves both language ids routed to it; routing identity stays `typescript` |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Live repro of the finding (section 1) after the change, following only mcpls's documented install command | `get_hover` returns `function f(a: string): number`; no initialize failure |
| SC-002 | Live repro with TypeScript 7 only and `typescript-language-server` configured | Error text names TypeScript 7 and lists both remedies; 0 occurrences of the bare upstream message as the only explanation |
| SC-003 | Native server via explicit config (control case) | Hover, definition, references and diagnostics each return a correct result |
| SC-004 | `get_diagnostics` on a file with a known type error, native server | The error is reported with correct 1-based range and message (FR-008) |
| SC-005 | Marker-script live test (US-004) with default config | No marker file created |
| SC-006 | User-defined TypeScript entry (US-005) | Spawned command and args byte-identical to config |
| SC-007 | Docs and `BuiltinServer::install_hint` agree on the TypeScript command | 100% (checked by test or checklist, NFR-009) |
| SC-008 | Type hierarchy tool on the native server | Typed unsupported-capability result, not a transport error |

### Live verification steps

All steps in a scratch directory outside the repository, with `tsconfig.json` and `a.ts`
containing `export function f(a: string): number { return a.length; }` plus a deliberate type error
(`const x: number = "s";`) in a second file.

1. Install per the documented command (FR-001) and run `get_hover` on `f`; expect the correct
   signature (SC-001).
2. Install `typescript@7` and `typescript-language-server`, configure
   `typescript-language-server` explicitly; expect the guidance error (SC-002).
3. Configure `tsc --lsp --stdio` explicitly; run `get_hover`, `get_definition`, `get_references`,
   `get_diagnostics` and a type hierarchy tool (SC-003, SC-004, SC-008).
4. Run with debug logging and confirm the native server accepts `initialize`, the configuration
   push and shutdown without protocol errors (FR-009).
5. If automatic selection is adopted: default config with global TypeScript 7, no `[[lsp_servers]]`
   entry; expect the native server to start and log its provenance (FR-007, FR-012).
6. Marker test: workspace-local `node_modules/.bin/tsc` that writes a marker file, TypeScript 7
   absent elsewhere; expect no marker (SC-005).
7. User override test: explicit entry pointing at a workspace-local `tsc`; expect it to run
   (SC-006) and be documented as the opt-in.
8. Update `.local/testing/` playbooks, `coverage-status.md` and `process-notes.md` per the
   continuous-improvement rules.

## 8. Agent Boundaries

### Always (without asking)
- Reproduce the finding in a scratch workspace outside the repository before changing behavior, and keep the repro as a regression case under `.local/testing/`.
- Express server flavor, detected package state and unresolved reasons as enums, per [[constitution]].
- Keep the `BuiltinServer` install hint, `README.md` and the user-guide pages consistent (NFR-009).
- Keep user-supplied `[[lsp_servers]]` entries and `initialization_options` authoritative.
- Run the full pre-commit suite and update `CHANGELOG.md` with the PR link.

### Ask First
- Enabling automatic native-server selection (FR-007) at all, versus guidance only (see section 9).
- Any source of consent for a workspace-local `tsc` other than the user's own explicit config.
- Adding a dependency for semver parsing or executable lookup (constitution VII).
- Choosing which `typescript` major range the install hint pins.
- Changing the existing tsserver pin semantics beyond reporting the new reason.

### Never
- Execute a `tsc` from a workspace `node_modules` by default.
- Hard-code an absolute path to TypeScript or its server.
- Interpolate server-supplied or workspace-supplied text into the added guidance.
- Match on the upstream error string as the only trigger for guidance without a typed detection ([NEEDS CLARIFICATION: see section 9]).
- Auto-install TypeScript or the server.
- Claim that the native server or the pin makes an untrusted workspace safe.

## 9. Open Questions

> [!question] Decisions needed before a plan
> - [NEEDS CLARIFICATION: scope decision, guidance only (FR-001 to FR-006, FR-008 to FR-010), or additionally automatic native-server selection (FR-007, FR-011 to FR-013)? Recommended for P1: ship the guidance and verification first as one change, then decide automatic selection after the live checks show the native server is a fully working default.]
> - [NEEDS CLARIFICATION: which `typescript` range does the install hint pin so that `typescript-language-server` works (the last JavaScript-based major, expected 6.x)? Confirm the exact range and that `typescript-language-server` 6.0.1 supports it, live.]
> - [NEEDS CLARIFICATION: which signal identifies the native package: `package.json` major version 7 or later, absence of `lib/tsserver.js`, or presence of `lib/tsc.js` and `bin/tsc`? Pre-release and `7.0.1-rc`-style versions and the `@typescript/native-preview` package (tsgo) need an explicit decision.]
> - [NEEDS CLARIFICATION: where does automatic selection happen, given the default config is static and project-marker heuristics decide only whether a server applies (see [[config/001-config-discovery-and-heuristics/spec|config/001]])? A second built-in entry, a resolution step at registration, or a spawn-time substitution each change the routing identity and the config-visible behavior differently.]
> - [NEEDS CLARIFICATION: what is the out-of-workspace lookup order for the native `tsc` (next to the resolved `typescript-language-server`, global npm prefix, `PATH`), and is a `PATH` entry that points into the workspace treated as workspace-supplied?]
> - [NEEDS CLARIFICATION: does the TypeScript 7 npm `bin/tsc` run a Node wrapper that starts a platform-specific native binary (`lib/getExePath.*`)? If so, is the native binary a separately installed optional-dependency package, and does resolution of that binary look inside the workspace?]
> - [NEEDS CLARIFICATION: workspace-code exposure of the native server itself: does it load tsconfig plugins or other workspace-supplied code, and does it have an upstream control equivalent to `tsserver.path`? Required before the `SECURITY.md` row is written (NFR-002); verify live as the typescript-language-server row was.]
> - [NEEDS CLARIFICATION: pull diagnostics against the native server: does it advertise `diagnosticProvider`, does it ever push, and does the existing pull-and-cache merge need a change (FR-008)? Live check required.]
> - [NEEDS CLARIFICATION: does the native server honor `workspace/didChangeConfiguration` and `workspace/configuration` as pushed by mcpls, and which settings keys does it read (FR-009)?]
> - [NEEDS CLARIFICATION: consent for a workspace-local `tsc`: is the user's explicit `[[lsp_servers]]` entry sufficient, or should it additionally require `--trust-project-config` when that entry comes from a project-local `mcpls.toml` (the existing gate already covers this, confirm no new mechanism is needed)?]
> - [NEEDS CLARIFICATION: should the guidance of FR-005 also be triggered by the upstream message alone when detection could not run (for example, launcher installs), accepting the fragility of matching text, or is typed detection the only trigger?]
> - [NEEDS CLARIFICATION: does the installed hint need an OS-specific variant on Windows, where the npm-installed server is a `.cmd` shim (existing `is_npm_package` note), and does the native `tsc` shim need the same?]
> - [NEEDS CLARIFICATION: issue number to record in the Metadata callout once filed.]

## 10. See Also

- [[constitution]] — project principles (type safety, security, simplicity)
- [[MOC-specs]] — all specifications
- [[config/001-config-discovery-and-heuristics/spec|config/001-config-discovery-and-heuristics]] — built-in server table, project-marker heuristics and the project-config trust gate
- [[lsp/006-server-spawn-install-hint/spec|lsp/006-server-spawn-install-hint]] — install hints and the spawn and init failure guidance this feature extends
- [[lsp/010-workspace-configuration-push/spec|lsp/010-workspace-configuration-push]] — configuration push that must be verified against the native server
- [[runtime/003-workspace-supplied-code-execution/spec|runtime/003-workspace-supplied-code-execution]] — trust model, tsserver pin and `SECURITY.md` rows this feature must not weaken
- [[runtime/004-server-text-hygiene/spec|runtime/004-server-text-hygiene]] — sanitization rules for server text in errors
- [[bridge/010-workspace-containment-single-predicate/spec|bridge/010-workspace-containment-single-predicate]] — the shared workspace-containment predicate for FR-011
- `crates/mcpls-core/src/config/server.rs` — `BuiltinServer::TypescriptLanguageServer`, `install_hint`, `early_exit_hint`, `is_npm_package`, `LspServerConfig::typescript()`
- `crates/mcpls-core/src/lsp/tsserver_pin.rs` — `UnresolvedReason`, `bundled_tsserver`, `pinned_initialization_options`
- `crates/mcpls-core/src/bridge/translator/diagnostics.rs` — pull and push diagnostics merge
- `crates/mcpls-core/src/error.rs` — `Error::LspInitFailed`, `Error::ServerNotFound` and their guidance
- `README.md` (TypeScript install block), `docs/user-guide/getting-started.md`, `docs/user-guide/configuration.md`, `SECURITY.md` (trust table and tsserver pin)
- `docs/benchmarks.md` — existing `tsgo --lsp --stdio` benchmark scenario, evidence that the native server works with the client
