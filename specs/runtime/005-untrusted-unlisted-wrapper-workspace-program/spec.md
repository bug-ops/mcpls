---
aliases:
  - Unlisted wrapper workspace program
  - Untrusted launcher generic backstop
tags:
  - sdd
  - spec
  - runtime
  - security
  - hardening
created: 2026-10-06
status: implemented
related:
  - "[[constitution]]"
  - "[[runtime/003-workspace-supplied-code-execution/spec|runtime/003-workspace-supplied-code-execution]]"
  - "[[runtime/004-server-text-hygiene/spec|runtime/004-server-text-hygiene]]"
---

# Feature: Untrusted mode refuses a workspace program started by an unlisted wrapper

> [!info] Metadata
> **Type**: enhancement (hardening)
> **Priority**: P3
> **Author**: Andrei G.
> **Source**: continuous-improvement cycle, live-test finding at HEAD e55ebc1
> **Issue**: #724

> [!abstract]
> In `--workspace-trust untrusted` mode the launcher analysis boundary-checks the program started by
> a closed table of wrappers (`time`, `nice`, `nohup`, `timeout`, `setsid`, `stdbuf`, `caffeinate`,
> `arch`, `env` and their `g`-prefixed, busybox, toybox and coreutils spellings). A wrapper outside
> the table is admitted without unwrapping, so a workspace executable named as one of its arguments
> runs from the workspace. This spec requires the "no executable from inside the workspace" boundary
> to hold regardless of which wrapper precedes the program, and weighs a generic argument-based
> backstop against extending the closed table.

## 1. Overview

### Problem Statement

`runtime/003` made untrusted mode refuse a server executable that lies inside a workspace root, and
(#686, #709/#710, #708/#719) extended that to programs a known wrapper starts. The wrapper table is
closed and `SECURITY.md` documents it as best-effort, naming `numactl`, `prlimit`, `ltrace`,
`valgrind`, `firejail`, `sg` and `run-parts` as admitted examples. Wrappers common on a stock macOS
host are not named, and are admitted.

**Reproduction (HEAD e55ebc1, release build with `--features transport-http`).** Config outside the
workspace, workspace root `<ws>`, executable fake language server `<ws>/bin/srv`. Run
`mcpls --config <cfg> --workspace-trust untrusted --allow-server fake`, set the server `command` and
`args` as below, call `get_hover` on a mapped file.

| `command` | `args` | Result |
|-----------|--------|--------|
| `nice` | `["<ws>/bin/srv"]` | refused: executable lies inside the workspace (as designed) |
| `xcrun` | `["<ws>/bin/srv"]` | admitted; server runs with argv[0] `<ws>/bin/srv` |
| `sandbox-exec` | `["-p", "(version 1)(allow default)", "<ws>/bin/srv"]` | admitted, same |
| `lockf` | `["-k", "/tmp/x.lock", "<ws>/bin/srv"]` | admitted, same |
| `taskpolicy` | `["-c", "utility", "<ws>/bin/srv"]` | admitted, same |
| `login` | `["-fq", "<user>", "<ws>/bin/srv"]` | admitted, same |
| `command` (POSIX utility) | `["<ws>/bin/srv"]` | admitted, same |

Controls: `script`, `chroot`, `sudo`, `strace`, `unshare`, `xargs` are refused as launchers;
`python3 <ws>/bin/srv` and `sh <ws>/bin/srv` run workspace scripts and are documented as not covered
("Interpreter arguments").

**Root cause.** Protection is keyed on the wrapper's name. Every newly noticed wrapper needs a table
entry; this is the third round.

### Goal

When untrusted mode admits a server, no argument of its configured command names an executable file
inside a workspace root that the child would start, whichever wrapper precedes it.

### Approaches Considered

| | A. Extend the closed table | B. Generic argument backstop | C. A and B together |
|---|---|---|---|
| Closes the reproduced wrappers | yes, one entry each (needs option grammar per wrapper) | yes, all at once | yes |
| Closes the next unnoticed wrapper | no | yes, when the program is given by path | yes |
| Option-grammar risk (misparse admits) | per wrapper | none | none |
| False refusals | none | an argument that is an executable file in the workspace but only data | same as B |
| Program given by bare name resolved from the child `PATH` | covered by the table's resolution | not covered | covered for listed wrappers |

**Decision:** approach C-minimal. The generic backstop (B) is added and the wrapper table stays
unchanged; FR-008 is not implemented because the backstop covers the reproduced wrappers.

### Out of Scope

- Interpreter script arguments (`python3 <ws>/x.py`, `node <ws>/cli.mjs`, `sh <ws>/x.sh`): remain
  documented as not covered, unless a requirement below covers a case (an executable script passed
  as an argument is refused under approach B or C only because it is an executable file in the
  workspace; non-executable scripts are not).
- A wrapper starting a program given by bare name that resolves through the child `PATH` to a
  workspace entry: the hardened `PATH` already excludes workspace, relative and empty entries.
- Wrappers that transform the argument before executing it (shell expansion, `eval`, `-c` strings).
- Trusted mode: neither analyzed nor rewritten.
- Config `env`, `cwd` or `initialization_options` content (covered by other requirements in `runtime/003`).

## 2. User Stories

### US-001: A wrapper outside the table cannot start a workspace executable

AS A user who runs mcpls on an untrusted checkout
I WANT a launch refused when any argument is an executable inside the workspace
SO THAT naming the program behind a wrapper I did not list does not bypass the boundary

```
GIVEN untrusted mode, allowed server "fake", workspace root <ws>, executable <ws>/bin/srv
WHEN command = "xcrun" and args = ["<ws>/bin/srv"]
THEN the server is refused as WorkspaceExecutable naming the offending path
AND no process is spawned
```

### US-002: Data paths into the workspace keep working

AS A user whose server reads a project config file
I WANT a non-executable workspace path in `args` to be admitted
SO THAT the backstop does not break ordinary configurations

```
GIVEN untrusted mode and <ws>/conf/server.json is a regular file without execute permission
WHEN command = "/usr/local/bin/fake-lsp" and args = ["--config", "<ws>/conf/server.json"]
THEN the server is admitted
```

### US-003: Documented behavior is accurate

AS A security reader
I WANT SECURITY.md to state exactly what the backstop does and does not cover
SO THAT I do not infer protection against interpreter arguments

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN untrusted mode plans an allowed server and any element of `args` resolves, as the child would resolve it, to an existing executable file whose canonical path lies inside a workspace root, THE SYSTEM SHALL refuse the server as `WorkspaceExecutable` and name the argument's index and resolved path, never its surrounding values. | must |
| FR-002 | THE SYSTEM SHALL resolve an argument for FR-001 against the spawn working directory (`ChildWorkingDir::Fixed`), following symlinks, so a relative argument and a symlink into the workspace are judged by the file the child would reach. | must |
| FR-003 | WHEN an argument is not an existing file, is a directory, or is a file without an execute permission bit (Windows: without an executable extension), THE SYSTEM SHALL NOT refuse it under FR-001. | must |
| FR-004 | WHEN an argument has the form `--option=<path>` or `-X<path>` THE SYSTEM SHALL NOT split it for FR-001 unless approach A or C requires it. Decision: the part after the first `=` is also a candidate path. | must |
| FR-005 | THE SYSTEM SHALL apply FR-001 only in untrusted mode, to the argument list as configured, before any rewrite, when the plan admits the server. Restart and respawn reuse the admitted configuration without re-planning, so the check is not repeated at spawn time: a refused server never becomes a `ServerInitConfig` and cannot be launched, but a file that becomes executable after planning is not caught (as with the server executable itself). | must |
| FR-006 | WHEN FR-001 refuses a server THE SYSTEM SHALL surface the refusal through the existing `UntrustedRefusal` path (startup failure, routing rebound, `Error::ServerFailedToStart` naming `--allow-server <id>` is not offered as an override). | must |
| FR-007 | THE SYSTEM SHALL keep the wrapper table's behavior for listed wrappers unchanged (option parsing, absolute-path rewrite, `=` refusal). | must |
| FR-008 | (not implemented: the backstop covers these wrappers) WHERE approach A or C is chosen THE SYSTEM SHALL add `xcrun`, `sandbox-exec`, `lockf`, `taskpolicy`, `login` and `command` to the closed table with a closed option grammar each. | won't |
| FR-009 | THE SYSTEM SHALL bound the filesystem work: at most one metadata lookup per argument and no directory traversal. | must |
| FR-010 | THE SYSTEM SHALL update `SECURITY.md` and add a `CHANGELOG.md` `[Unreleased]` entry marked breaking. | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Security | The check runs on the canonical path and mirrors `lsp/command_path.rs` resolution, so symlink and `..` forms are judged by target. |
| NFR-002 | Security | A lookup error other than not-found (permission denied on a workspace path) refuses the server only when the lexically normalized path lies inside the workspace boundary (fail closed); any other lookup error means the text is not a candidate. |
| NFR-003 | Compatibility | Trusted mode behavior and the tool surface are unchanged. Untrusted-mode refusals are breaking for configs passing an executable workspace file as data. |
| NFR-004 | Performance | Planning cost is O(number of args) metadata calls; no impact on request latency. |
| NFR-005 | Portability | Tests run on Linux, macOS and Windows; fake executables use `.exe` names on Windows. |
| NFR-006 | Privacy | Refusal text names the path and index, not argument values that are not paths. |

## 5. Data Model

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| `UntrustedRefusal::WorkspaceExecutableArgument` | Refusal for an argument that is an executable inside a root | argument index, canonical path |
| `UntrustedRefusal::UnreadableWorkspaceArgument` | Refusal for an argument inside a root that cannot be read (permission denied) | argument index, normalized path |
| Candidate argument | An element of `args` examined by the backstop | index, resolved canonical path, is-executable flag |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Argument is a workspace executable also reachable via a symlink outside the root | refused by canonical target |
| Argument is a symlink in the workspace to an executable outside | admitted (target outside) |
| Argument is a relative path resolving into the workspace from the fixed cwd | refused if it resolves to an executable there |
| Argument is a non-executable workspace file (config, script without `x` bit) | admitted |
| Argument is an executable data file (checked-in script with `x` bit used as a config) | refused (documented false positive) |
| Argument is a directory | admitted |
| Argument contains NUL or is not UTF-8 | existing `NonUtf8Path` handling; never panics |
| Argument is a flag value such as `-p "(version 1)"` | not a file; admitted |
| Bare name `srv` with `<ws>` on the child `PATH` | not covered by the backstop; hardened `PATH` excludes workspace |
| `python3 <ws>/bin/srv` where `srv` is executable | refused as a side effect of FR-001; non-executable script still admitted |
| Server also listed wrapper (`nice <ws>/bin/srv`) | refused by the table; the backstop does not change the message |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Reproduction rows above (`xcrun`, `sandbox-exec`, `lockf`, `taskpolicy`, `login`, `command`) | all refused, none spawn |
| SC-002 | Controls `nice`, `script`, `chroot`, `sudo`, `strace`, `unshare`, `xargs` | unchanged outcome |
| SC-003 | Non-executable workspace data file in `args` | admitted |
| SC-004 | Trusted mode with the same configs | unchanged, admitted |
| SC-005 | Restart and respawn after a refusal | no spawn (a refused server has no init config; admitted servers are not re-checked) |

## 8. Agent Boundaries

### Always (without asking)
- Add unit tests for each acceptance scenario and a live repro per the testing playbook.
- Keep refusals typed (`UntrustedRefusal`), not stringly-typed.
- Update `.local/testing/` playbooks, coverage-status and regression notes for the changed behavior.

### Ask First
- Choosing approach A, B or C.
- Any new CLI flag or config key (none is expected; untrusted mode is command-line only).
- Splitting `--opt=<path>` arguments (FR-004).

### Never
- Add an environment variable or config key that relaxes the check.
- Make `--allow-server` override the refusal.
- Modify trusted-mode behavior.

## 9. Open Questions

> [!success] Resolved
> - Approach: C-minimal (the generic backstop, the wrapper table unchanged).
> - FR-004: the text after the first `=` is also checked.
> - NFR-002: fail closed only on permission denied inside the boundary; any other lookup error is not a candidate.
> - Executable data files in `args`: the false-positive cost is accepted and documented.
> - Scope: the backstop covers all top-level `args` as configured, not the arguments of a listed wrapper's program.

## 10. Documentation Impact

- `SECURITY.md` (Launcher bullet): replace "is admitted without unwrapping it, so the program it starts is not checked" with the backstop semantics: any argument resolving to an executable file inside the workspace refuses the server; non-executable data paths are admitted; a program given by bare name and interpreter scripts without an execute bit remain uncovered. Keep the closed-list wording for shells and interpreters.
- `SECURITY.md` "What the mode does not cover": narrow "Interpreter arguments" to non-executable scripts.
- `CHANGELOG.md` `[Unreleased]`: one line, marked **Breaking:**, ending with the PR link.
- `book/src/advanced/untrusted-mode.md`: mirror the SECURITY.md change.

## 11. See Also

- [[constitution]] — project principles
- [[MOC-specs]] — all specifications
- [[runtime/003-workspace-supplied-code-execution/spec|runtime/003]] — untrusted mode, launcher analysis, workspace executable refusal
- [[runtime/004-server-text-hygiene/spec|runtime/004]] — handling of attacker-influenced text
- Code: `crates/mcpls-core/src/lsp/launcher.rs` (`EXEC_WRAPPERS`), `crates/mcpls-core/src/runtime/untrusted.rs` (`vetted_wrapped_programs`), `crates/mcpls-core/src/lsp/command_path.rs`
