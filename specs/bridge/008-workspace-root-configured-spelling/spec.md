---
aliases:
  - Workspace root configured spelling
  - Symlinked workspace root regression
tags:
  - sdd
  - spec
  - bridge
  - config
  - security
  - regression
created: 2026-10-04
status: implemented
related:
  - "[[constitution]]"
  - "[[config/001-config-discovery-and-heuristics/spec|config-discovery-and-heuristics]]"
  - "[[mcp/002-mcp-resources-diagnostics/spec|mcp-resources-diagnostics]]"
---

# Feature: Admit a Workspace Root Under Its Configured Spelling When the Config Is Loaded From a File

> [!info] Metadata
> **Type**: bug (regression)
> **Priority**: P1
> **Source**: continuous-improvement live-testing finding (cycle 037), reproduced on the release binary at `ad90190`
> **Related issues**: #571, #579 (section 12). Regresses #533 and #552 (`WorkspaceRoots`, commits `4f06dfc` and `ad90190`); last good release `47bbcda`.

## 1. Overview

### Problem Statement

#533/#552 introduced `WorkspaceRoots` (`crates/mcpls-core/src/bridge/workspace_roots.rs`). A tool
path is accepted when it lies under (a) the root's canonical, symlink-free path, (b) the root
exactly as configured, or (c) the logical `$PWD` when it names the real working directory. This
is the documented contract (`docs/user-guide/tools-reference.md`, "file_path"; CHANGELOG #533,
#552). Condition (b) is lost for every root that comes from a TOML config file:

1. `ServerConfig::load_from` (`config/mod.rs`, `load_from_with_root_base`) overwrites
   `config.workspace.roots` in place with the output of `canonicalize_workspace_roots` /
   `resolve_workspace_roots`. The configured spelling is discarded at load time.
2. `serve_with` then calls `build_workspace_roots(&config.workspace.roots)` (`lib.rs`). The roots
   are already absolute and canonical, so it takes the all-absolute branch and passes them as
   both the canonical roots and the aliases. `WorkspaceRoots::new` drops every alias equal to a
   canonical root, so the alias set ends up empty.

Result: a client that names files through a symlinked spelling of the root receives
`-32602 path outside workspace: <path>` from every path-taking tool and from `resources/subscribe`.
On macOS `/tmp` and `/var` are symlinks to `/private/tmp` and `/private/var`, and projects reached
through a symlinked directory behave the same way. Release `47bbcda` accepted these paths, so this
is a regression and contradicts the documented contract. Only configs built in memory by a caller
(`mcpls-bench`, library embedders) keep the alias, because they skip `load_from`. The shipped
binary always loads a TOML file, so the only supported entry point is the broken one.

The existing tests do not catch it. `WorkspaceRoots::resolve` and `build_workspace_roots` are
tested with raw symlinked roots, and the `load_from` tests assert that the loaded roots are
canonical. No test exercises `load_from` followed by `build_workspace_roots` followed by
`validate_path_against_roots`.

> [!bug] Reproduction (release binary at `ad90190`)
> 1. `cp -R <rust project> /tmp/ci037_var`; config `[workspace] roots = ["/tmp/ci037_var"]` plus a
>    rust-analyzer `[[lsp_servers]]` entry.
> 2. `mcpls --config cfg.toml`, MCP `initialize`, then `tools/call get_hover`
>    `{file_path: "/tmp/ci037_var/src/main.rs", line: 5, character: 8}`.
>    Actual: `{"error":{"code":-32602,"message":"path outside workspace: /tmp/ci037_var/src/main.rs"}}`.
> 3. Same call with `/private/tmp/ci037_var/src/main.rs` succeeds.
> 4. With `roots = [".../scratch/proj037_link"]`, where `proj037_link` is a symlink to `proj037`:
>    `get_hover`, `get_cached_diagnostics`, `get_references` and `resources/subscribe` on
>    `.../proj037_link/src/main.rs` all fail.

### Goal

Every workspace root stays addressable by the spelling the operator wrote (or the logical `$PWD`
spelling, for cwd-derived roots), whether the config came from a TOML file, from the auto-discovered
global config, or from a caller-built `ServerConfig`. Symlink-escape protection is unchanged:
the physical path must still lie under a canonical root.

### Out of Scope

- Admitting spellings that reach a root only through some other symlink (an unrelated link to the
  root, or a second symlink hop that was never configured). The documented narrowing from #533 stays.
- Changing how roots are canonicalized, how LSP `rootUri` / workspace folders are derived, how
  diagnostics are keyed, or how project-marker heuristics are applied. All of these stay canonical.
- Replacing the lexical-prefilter plus canonical-containment design of `validate_path_against_roots`.
- Hot-reloading or re-resolving roots after startup (roots remain fixed at startup).
- Windows case-folding and verbatim-prefix rules. They are already handled by `CaseRule` and
  `prefix_eq` and are not touched.

## 2. User Stories

### US-001: Client names the root as configured

AS A client of mcpls (an AI agent or MCP host) launched with a config file
I WANT TO pass `file_path` values spelled like the `workspace.roots` entry in that file
SO THAT my tool calls work on macOS temp directories and symlinked project directories as they did
before #552.

**Acceptance criteria:**

```
GIVEN a config file with roots = ["/tmp/ws"] where /tmp/ws resolves to /private/tmp/ws
  AND the server is running
WHEN get_hover is called with file_path "/tmp/ws/src/main.rs"
THEN the call is dispatched (not rejected with PathOutsideWorkspace)
```

```
GIVEN the same server
WHEN get_hover is called with file_path "/private/tmp/ws/src/main.rs"
THEN the call is dispatched (canonical spelling still works)
```

### US-002: Operator uses a relative root in a config file

AS AN operator who writes `roots = ["."]` or `roots = ["proj_link"]` in a config file
I WANT clients to be able to name the resolved root by the spelling that results from joining the
relative entry to the config directory (or the cwd, for the auto-discovered global config)
SO THAT a relative root behaves like the equivalent absolute root.

**Acceptance criteria:**

```
GIVEN /work/cfgdir/mcpls.toml with roots = ["proj_link"], and /work/cfgdir/proj_link a symlink to /work/real
WHEN a tool is called with file_path "/work/cfgdir/proj_link/src/lib.rs"
THEN it is admitted
```

### US-003: Escape protection is preserved

AS A security-conscious operator
I WANT a symlink inside a workspace root that points outside it to stay rejected, even when the
path is spelled through the configured alias
SO THAT widening the accepted spellings never widens the accessible file set.

**Acceptance criteria:**

```
GIVEN roots = ["/tmp/ws"] (alias of canonical /private/tmp/ws)
  AND /tmp/ws/out is a symlink to /etc
WHEN a tool is called with file_path "/tmp/ws/out/passwd"
THEN the call is rejected with PathOutsideWorkspace
```

### US-004: Embedder-built configs behave the same

AS A library embedder calling `serve` / `serve_with` with a `ServerConfig` built in code
I WANT the same set of accepted spellings as a file-loaded config
SO THAT behavior does not depend on how the config was produced.

**Acceptance criteria:**

```
GIVEN a caller-built ServerConfig with absolute workspace.roots = [/tmp/ws] (symlinked)
WHEN a tool is called with file_path "/tmp/ws/src/main.rs"
THEN it is admitted, identically to the file-loaded config
```

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN the server starts from a config loaded from a TOML file (explicit `--config`, `$MCPLS_CONFIG`, trusted project-local `mcpls.toml`, or the auto-discovered global config) THE SYSTEM SHALL admit a path that lies under an absolute `workspace.roots` entry exactly as written in that file, in addition to the canonical root | must |
| FR-002 | WHEN a config file contains a relative `workspace.roots` entry THE SYSTEM SHALL admit paths under the absolute spelling formed by joining the entry to its resolution base as written (the config file's directory for the explicit and project-local configs, the process cwd for the auto-discovered global config), in addition to the canonical root | must |
| FR-003 | WHEN `workspace.roots` is empty (cwd default) or a relative root is resolved against the cwd THE SYSTEM SHALL also admit the logical `$PWD` spelling, only when `$PWD` is absolute and names the same directory as the real cwd | must |
| FR-004 | WHEN the server starts from a caller-built `ServerConfig` (`serve`, `serve_with`, `mcpls-bench`) THE SYSTEM SHALL admit the same spellings as in FR-001 through FR-003, with no dependence on whether the config passed through `load_from` | must |
| FR-005 | THE SYSTEM SHALL decide admission in two steps: a lexical pre-check against the canonical roots and their aliases, then a physical check that the path's canonical form lies under a canonical root. An alias SHALL never admit a path whose canonical form is outside every canonical root. This covers client-supplied tool paths; server-supplied edit URIs follow the same two steps with a stricter lexical step ([[bridge/010-workspace-containment-single-predicate/spec|010]], FR-008) | must |
| FR-006 | WHEN a path reaches a root through a symlink located inside the root that points outside it THE SYSTEM SHALL reject it with `PathOutsideWorkspace`, whether the path is spelled with the canonical form or any alias | must |
| FR-007 | THE SYSTEM SHALL keep canonical roots as the only form used for LSP `rootUri` and workspace folders, project-marker heuristics (`should_spawn`), diagnostics keying and filtering, and any other consumer that compares physical paths | must |
| FR-008 | THE SYSTEM SHALL apply the same admitted spellings to every path-taking tool and to `resources/subscribe` and the diagnostics resource URI, since all of them validate against the one `WorkspaceRoots` built at startup | must |
| FR-009 | WHEN a configured root cannot be canonicalized (absolute root that does not yet exist) THE SYSTEM SHALL keep the existing fallback (canonical-existing-prefix form) and SHALL still admit the as-written spelling | should |
| FR-010 | THE SYSTEM SHALL keep rejecting, before any filesystem access, a path that matches no canonical root and no alias, so the error does not reveal whether such a file exists (#533) | must |
| FR-011 | THE configured spelling SHALL survive the load-to-serve hand-off as typed data in the config layer. It SHALL NOT be recovered by re-reading the file or by string heuristics over the canonical root | must |
| FR-012 | WHEN the configured spelling equals the canonical root (no symlink involved) THE SYSTEM SHALL behave exactly as before, with no alias recorded | must |
| FR-013 | WHEN the config file path is relative THE SYSTEM SHALL keep the rebased relative roots relative (a config path that is relative but drive- or root-qualified is made absolute first), so `WorkspaceRoots::from_configured` records the validated logical `$PWD` spelling | must |
| FR-014 | THE SYSTEM SHALL record an alias only when the alias, in the exact form stored (simplified and lexically normalized), resolves on its longest existing prefix to the canonical path of its own root; a dropped alias is logged at debug level. This keeps `..` after a symlink from admitting an unrelated tree and from reopening the #533 existence oracle (FR-010) | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Security | The set of files reachable through any tool SHALL be identical before and after this change. Only the set of accepted spellings of already-reachable paths grows. |
| NFR-002 | Security | No new filesystem access for out-of-workspace paths: the lexical pre-check keeps rejecting them without a `stat`. |
| NFR-003 | Performance | Alias computation is done once at startup; the per-call path adds no filesystem work beyond today's single `canonicalize`. |
| NFR-004 | Type safety | Canonical and as-configured roots SHALL be distinct types (or a single type with named fields), not two interchangeable `Vec<PathBuf>`. A canonical-only consumer must not be able to receive an unresolved path by mistake. |
| NFR-005 | API stability | Pre-1.0: breaking changes to `ServerConfig` / `WorkspaceConfig` are acceptable if needed, and SHALL be recorded in CHANGELOG as breaking. TOML schema SHALL NOT change. |
| NFR-006 | Portability | Behavior SHALL hold on Linux, macOS and Windows. Tests that need a symlink are `#[cfg(unix)]`; a non-unix test covers the plain-alias path. |
| NFR-007 | Documentation | `tools-reference.md` contract stays as written. Rustdoc of the touched public items states where the configured spelling is preserved. |

## 5. Data Model

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| Configured root | One `workspace.roots` entry after relative resolution but before canonicalization | absolute path, as-written spelling, origin (file, caller-built) |
| Canonical root | Symlink-free form of a configured root | absolute canonical path |
| Root alias | A lexical spelling a client may use for a root: configured form, config-dir-joined form, logical `$PWD`-joined form | absolute, lexically normalized, distinct from every canonical root |
| `WorkspaceRoots` | Immutable set of canonical roots plus aliases shared by all validation sites | `canonical`, `aliases` |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Root configured through a symlink; client uses the canonical path | Admitted (FR-001) |
| Root configured through a symlink; client uses the configured path | Admitted (FR-001, this bug) |
| Symlink inside the root points outside; path uses canonical root prefix | Rejected `PathOutsideWorkspace` (FR-006) |
| Symlink inside the root points outside; path uses the alias prefix | Rejected `PathOutsideWorkspace` (FR-006) |
| Symlink inside the root points to another directory still inside the same root | Admitted (canonical form lies under the root) |
| Symlink inside root A points into root B (both configured) | Admitted (canonical form lies under B) |
| `<alias>/../x` where the alias is a symlink | Rejected lexically: `..` is resolved before the alias comparison, so the result no longer has the alias prefix. Conservative, unchanged from today |
| Sibling directory sharing a name prefix (`/tmp/ws2` vs root `/tmp/ws`) | Rejected: comparison is by path component, not string prefix |
| Root symlink retargeted after startup | Canonical roots are fixed at startup, so the alias then resolves outside every canonical root and is rejected by the physical check (FR-005) |
| Alias path names a file that does not exist | `Error::FileIo` from canonicalization, as for any admitted-but-missing path |
| Absolute root that does not exist at startup | Existing fallback root kept; as-written spelling admitted (FR-009) |
| Two roots whose aliases or canonical forms coincide | Duplicates dropped when the root set is built; no error |
| `$PWD` unset, relative, or naming a different directory | No `$PWD` alias (FR-003); canonical and config-dir aliases still apply |
| Relative root, config path given relative (`--config mcpls.toml`, trusted project-local `mcpls.toml`) | The entry is joined to the relative config directory and stays relative, so startup resolution adds the logical `$PWD` spelling (FR-013). An absolute config path gives an absolute joined spelling and no `$PWD` alias |
| Alias that climbs out of its root through a symlink (`roots = [".."]` in a config reached through a symlinked directory) | Not recorded (FR-014): the lexical parent is a different tree than the physical one |
| Windows verbatim prefix or drive-letter case differences | Handled by existing `CaseRule` / `prefix_eq`; not changed |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Step 2 of the reproduction (`get_hover` through `/tmp/ci037_var/...`) on a release build | Returns hover content; no `-32602` |
| SC-002 | `get_hover`, `get_cached_diagnostics`, `get_references`, `resources/subscribe` through a `proj037_link` symlinked root | All admitted |
| SC-003 | Symlink-escape suite (inside-root link to outside, canonical and alias spelling) | 100% rejected, error is `PathOutsideWorkspace` |
| SC-004 | File-loaded and caller-built configs for the same root | Identical accept/reject decisions across the matrix in section 8 |
| SC-005 | Existing `WorkspaceRoots`, `validate_path_against_roots`, `load_from` and `build_workspace_roots` unit tests | Pass unchanged, or are updated only where FR-011 forces a type change |

## 8. Acceptance and Regression-Test Expectations

The missing coverage is the integration across `load_from`, `build_workspace_roots` and
`validate_path_against_roots`. Add tests at that seam first; they must fail on `ad90190` and pass after the fix.

| ID | Test (Unix unless noted) | Asserts |
|----|--------------------------|---------|
| RT-001 | Write a TOML file with an absolute symlinked root, call `ServerConfig::load_from`, build the roots exactly as `serve_with` does, then `validate_path_against_roots` on `<link>/src/main.rs` | Ok, returns the canonical path |
| RT-002 | Same setup, canonical-spelling path | Ok |
| RT-003 | Same setup, with `<link>/out` -> a directory outside the root; validate `<link>/out/f` and `<canonical>/out/f` | Both `PathOutsideWorkspace` |
| RT-004 | Relative root in a file (`roots = ["link"]`, `link` -> real) with `ConfigDir` base | `<config_dir>/link/f` admitted |
| RT-005 | Relative root with `RelativeRootBase::Cwd` (global config path) | `<cwd>/link/f` admitted; with a valid logical `$PWD`, `<$PWD>/link/f` admitted |
| RT-006 | Caller-built `ServerConfig` with the same absolute symlinked root, no `load_from` | Same result as RT-001 |
| RT-007 | `<alias>/../x` and sibling `<alias>2/f` | Rejected without filesystem access |
| RT-008 | Empty `roots` with a valid and with a forged `$PWD` | Valid: logical spelling admitted. Forged: not admitted |
| RT-009 | Non-symlinked root | No alias recorded, behavior identical to today (FR-012) |
| RT-010 | End-to-end (MCP stdio, mock or rust-analyzer fixture): config with a symlinked root, `get_hover`, `get_cached_diagnostics`, `resources/subscribe` through the configured spelling | No `-32602` |
| RT-011 | `load_from` tests that assert canonical roots (`test_load_from_resolves_relative_roots_against_config_directory` and similar) | Still assert the canonical roots that consumers receive (FR-007), or are rewritten against the new typed accessor |

Add a live-testing playbook entry under `.local/testing/playbooks/` for the macOS `/tmp` and
`/var` case and update `coverage-status.md` for the `config` and `bridge` subsystems, per
`.claude/rules/continuous-improvement.md`.

## 9. Design Notes

> [!success] Decision
> Option A. `ServerConfig::load_from` keeps `workspace.roots` as written: absolute entries verbatim,
> relative entries joined to the config directory as given (absolute config path) or kept relative
> (relative config path, and the global config tier, which resolves against the cwd). Every rebased
> root must exist at load time. `WorkspaceRoots::from_configured` is the single place that
> canonicalizes and derives aliases, for file-loaded and caller-built configs alike. No typed pair
> was added to the config (B) and `load_from`'s signature is unchanged (C). The decision made in
> the plan phase is recorded below for reference.

The requirement is FR-011: the configured spelling must reach `build_workspace_roots`. Options:

| Option | Sketch | Trade-off |
|--------|--------|-----------|
| A. `load_from` stops rewriting roots | Keep raw absolute-ified roots in `ServerConfig`; canonicalize only in `build_workspace_roots` | Smallest change, but `config.workspace.roots` then holds non-canonical paths, so every other reader and the `load_from` tests must be re-audited (FR-007). Relative-root errors move from load time to serve time |
| B. Typed pair in config | `WorkspaceConfig` holds a resolved-roots type carrying both canonical and as-configured paths (skipped by serde); `build_workspace_roots` consumes it | Satisfies NFR-004 and keeps load-time errors. Larger API change; needs a story for caller-built configs that only set `roots` |
| C. Build `WorkspaceRoots` at load time | `load_from` returns it alongside the config | Splits config from its roots and changes the `load_from` signature |

Prefer B or A on type-safety grounds. Whichever is chosen, also review `resolve_workspace_roots`
and `canonicalize_workspace_roots` for the load-time path: they currently return only canonical forms
and discard the joined, pre-canonical spelling that FR-002 needs. [NEEDS CLARIFICATION: choose
between A, B and C. B is the recommendation of this spec but changes a public config type.]

## 10. Agent Boundaries

### Always (without asking)

- Write the RT-001 through RT-006 failing tests first and confirm they fail on current `main`
- Keep canonical roots as the sole input to LSP init, heuristics and diagnostics (FR-007)
- Run the four pre-commit gates from `CLAUDE.md`
- Add a CHANGELOG entry under `[Unreleased]` with the PR link

### Ask first

- Changing the public shape of `ServerConfig`, `WorkspaceConfig` or `load_from`'s signature
- Admitting any spelling beyond those listed in FR-001 through FR-003
- Touching `validate_path_against_roots`'s two-step algorithm

### Never

- Accept a path whose canonical form is outside every canonical root
- Add filesystem access to the lexical pre-check
- Edit anything under `crates/` as part of writing this spec; only the implementation phase may
- Weaken typing (stringly-typed roots, untyped maps) to carry the configured spelling

## 11. Open Questions

> [!question] Resolved
> - Option A (section 9).
> - A ConfigDir-relative root admits the logical `$PWD` spelling when the config path is relative (FR-013); an absolute config path does not add it.
> - No startup log of accepted spellings; only dropped aliases are logged, at debug level (FR-014).
> - Whether a patch release is needed is left to the maintainer.

> [!warning] Known limitation
> Zero-config roots (empty `workspace.roots`) admit only the physical working directory and the validated `$PWD`. A client naming the directory through another symlinked spelling is not admitted; this follows the documented contract and is tracked in #579.

## 12. Amendment: Root-Level System Symlink Aliases (#579)

Split out of #571: a client that spawns mcpls with a cwd option (so `$PWD` is unset or differs) and
names files through `/tmp/...` on macOS was rejected, because `/tmp` is a symlink to `/private/tmp`
and no configured or logical spelling covers it. Status: implemented.

| ID | Requirement | Priority |
|----|------------|----------|
| FR-018 | THE SYSTEM SHALL, once at startup, admit aliases of each canonical root that differ from it only through a symlink located directly under the filesystem root (`/tmp`, `/var`), in addition to the spellings of FR-001 through FR-003 (`WorkspaceRoots::from_configured_with` adds them after the configured and logical aliases) | must |
| FR-019 | Each such alias SHALL be admitted only if `canonicalize(alias)` equals the root it was derived from. A forged or retargeted link therefore admits nothing | must |
| FR-020 | Discovery SHALL list `/` without following links (`DirEntry::file_type`, `read_link`) and SHALL resolve each target lexically against `/`. A link is a candidate for a root only when its target is a component prefix of that root; chained links are skipped. Only the constructed alias is canonicalized, so unrelated links (automounts, network mounts) are never touched | must |
| FR-021 | Any `read_dir("/")` or `read_link` failure SHALL yield no aliases, be logged at debug level, and SHALL NOT fail startup (#348) | must |
| FR-022 | THE SYSTEM SHALL keep the lexical pre-check and the canonical containment check unchanged. Edit URIs from servers stay behind `contains_canonical` | must |

Non-goals: links not located directly under `/` (`~/link`) stay unadmitted unless configured.
`Location.out_of_workspace` goes through the same admitted spellings (`admits_uri`), so a
`/tmp`-spelled server location reads `out_of_workspace: false` (#605). Non-Unix platforms add no
system aliases.

Tests: a unix symlink-table test over a tempdir standing in for `/`, a forged-alias test
(`p/a/../b` lexically vs physically), an unreadable-directory test, `..`/sibling rejection without
a stat, and a macOS test over a real `/tmp` directory.

## 13. See Also

- [[constitution]] — project principles
- [[MOC-specs]] — all specifications
- [[config/001-config-discovery-and-heuristics/spec|config-discovery-and-heuristics]] — how `workspace.roots` is loaded and resolved (#345, #348)
- [[mcp/002-mcp-resources-diagnostics/spec|mcp-resources-diagnostics]] — `resources/subscribe` and the diagnostics URI also depend on `WorkspaceRoots`
- Code: `crates/mcpls-core/src/bridge/workspace_roots.rs`, `crates/mcpls-core/src/bridge/translator/routing.rs` (`validate_path_against_roots`), `crates/mcpls-core/src/config/mod.rs` (`load_from_with_root_base`)
- Docs: `docs/user-guide/tools-reference.md` ("file_path"), CHANGELOG entries for #533 and #552
