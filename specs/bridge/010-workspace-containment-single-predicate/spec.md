---
aliases:
  - Workspace containment single predicate
  - WorkspaceRoots single constructor
tags:
  - sdd
  - spec
  - bridge
  - refactor
  - security
created: 2026-10-04
status: implemented
related:
  - "[[constitution]]"
  - "[[bridge/008-workspace-root-configured-spelling/spec|workspace-root-configured-spelling]]"
  - "[[mcp/002-mcp-resources-diagnostics/spec|mcp-resources-diagnostics]]"
---

# Feature: One Containment Predicate and One Constructor for Workspace Roots

> [!info] Metadata
> **Type**: refactor with a behavior fix
> **Priority**: P2
> **Related issues**: #558. Follows #533 and #552 (`WorkspaceRoots`); shares code with [[bridge/008-workspace-root-configured-spelling/spec|008]] (#571).

## 1. Overview

### Problem Statement

After #552, workspace containment and root construction are split across three modules and the rules
have drifted apart.

Three containment predicates with different rules:

| Predicate | Aliases | `C:` vs `\\?\C:` / UNC | Rejects `.`/`..` |
|-----------|---------|------------------------|------------------|
| `WorkspaceRoots::admits_lexically` / `contains_canonical` | yes | yes | caller normalizes |
| `bridge::uri_in_workspace_roots(&Uri, &[PathBuf])` | no | no | yes |
| `lib.rs::diagnostic_path_in_workspace` | yes | yes | yes |

Callers of the slice-based predicate had to drop the `WorkspaceRoots` type (`EncodingCtx` held
`Arc<[PathBuf]>`, `WorkspaceEditConverter` held `&[PathBuf]`). Consequences: rename and code-action
edits using an alias spelling of a root were dropped although `validate_path_against_roots` accepts
them; `out_of_workspace` was `true` for such locations; on Windows a verbatim-prefixed canonical root
failed `Path::starts_with` against a plain URI path.

Two root constructors with different guarantees: the production `build_workspace_roots` family in
`lib.rs`, and the public `WorkspaceRoots::resolve`, which kept a non-existent relative root verbatim
in the canonical list and was the only constructor the test fixtures exercised.

### Goal

`WorkspaceRoots` owns both concerns: a single fallible production constructor, and one set of URI
predicates: a lexical pre-filter (`admits_uri`) and a strict write gate (`admits_edit_uri`).

### Out of Scope

- Changing which spellings of a root are admitted (see 008).
- Making read-only navigation results filtered by containment.

## 2. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | THE SYSTEM SHALL build every production `WorkspaceRoots` through `WorkspaceRoots::from_configured`; `WorkspaceRoots::resolve` and `canonical_shared` SHALL NOT exist | must |
| FR-002 | `from_configured` SHALL resolve relative entries against the process working directory, treat an empty list as the working directory, fail with `InvalidConfig` for a relative root that cannot be canonicalized, and read the working directory only when a root needs it (#348) | must |
| FR-003 | THE SYSTEM SHALL record an alias (configured, working-directory or logical `$PWD` spelling) only when its stored form resolves, on the existing prefix, to the canonical path of its own root; a dropped alias SHALL be logged at debug level | must |
| FR-004 | `WorkspaceRoots::admits_uri` SHALL be the only lexical URI predicate and is a pre-filter, never a trust boundary: it accepts an absolute `file:` path without `.`/`..` components that lies under a canonical root or an alias with the platform case rule, never touches the filesystem, and rejects everything when no root is configured | must |
| FR-005 | THE diagnostics pump (followed by `PublishedDiagnosticsUri::resolve`'s canonical check) and `EncodingCtx::is_out_of_workspace` (advisory) SHALL call `admits_uri`, the edit converter SHALL call `admits_edit_uri`, and all SHALL hold a `WorkspaceRoots`, not a path slice | must |
| FR-006 | `PublishedDiagnosticsUri::resolve` SHALL take `&WorkspaceRoots` and decide containment on the canonical path with `contains_canonical` | must |
| FR-007 | Test fixtures SHALL build roots through `from_configured` for real directories, and through a `cfg(test)` constructor (`for_test`) for pure-lexical unit tests that name literal paths | must |
| FR-008 | WHEN a server supplies a URI that mcpls would write through (rename and code-action edits) THE SYSTEM SHALL use `WorkspaceRoots::admits_edit_uri`: exact component comparison against the canonical roots and aliases (no case folding) AND a canonical form of the path (longest existing prefix resolved) under a canonical root. Aliases are then only a spelling aid | must |
| FR-009 | THE SYSTEM SHALL canonicalize a path through one routine, `canonicalize_existing_prefix`, which falls through to the next ancestor only on `NotFound` and `NotADirectory`, reports every other error as `Unresolved::Transient` and refuses `..` components; `admits_edit_uri` SHALL treat any `Unresolved` as not admitted | must |
| FR-010 | THE SYSTEM SHALL validate client paths only through `WorkspaceRoots::validate` (async, canonicalization on the blocking pool) or `validate_blocking`, which return a `WorkspacePath`; a handler validates a request path once and passes the `WorkspacePath` on | must |
| FR-011 | THE tsserver pin SHALL decide whether the pinned tsserver lies inside the workspace with `WorkspaceRoots::contains_canonical` on the canonicalized tsserver path | must |
| FR-012 | `resources::make_uri` SHALL encode through the same `file_url` as `try_path_to_uri`, so every path the bridge can open has a resource URI on Windows | must |

## 3. Behavior Changes

| Area | Before | After |
|------|--------|-------|
| Rename and code-action edits | Dropped when the URI used an alias spelling of a root | Admitted |
| `out_of_workspace` | `true` for alias spellings | `false` for any admitted alias |
| Windows verbatim and UNC roots | Edit gate and `out_of_workspace` failed `starts_with` | Handled by `prefix_eq` |
| Edit gate | Exact lexical, no aliases | Exact-case containment under a root or alias AND canonical containment; alias spellings are kept, escaping or retargeted symlinks and case variants are dropped |
| `tool_surface.json` | `out_of_workspace` description named an internal function | Description restated without internal names |

## 4. Edge Cases

| Scenario | Expected Behavior |
|----------|-------------------|
| `file:///ws/../etc/passwd` | Rejected (`..` component) |
| `file:///ws2/a.rs` for root `/ws` | Rejected (component comparison) |
| Alias that climbs out of its root through a symlink (`link/..`) | Dropped at construction; the existence oracle closed in #533 stays closed |
| Absolute root that does not exist yet | Longest-existing-prefix canonical form; as-written alias kept |
| Empty root list | `admits_uri` is false for every URI |

## 5. Acceptance and Regression-Test Expectations

| ID | Test | Asserts |
|----|------|---------|
| RT-001 | `admits_uri` unit tests: empty roots, under root, alias, outside, non-`file:`, `..`, sibling prefix, Windows verbatim disk and UNC | Table in section 3 |
| RT-002 | `from_configured`: missing absolute root, missing relative root, unreadable cwd only when needed, dot and parent roots, unicode and spaces | FR-002 |
| RT-003 | Symlinked root keeps its configured alias; `link/..` alias dropped; alias under a symlinked ancestor kept | FR-003 |
| RT-004 | Logical `$PWD` accepted only when it names the cwd (injected through `ProcessCwd`) | FR-003 |
| RT-005 | Edit and `out_of_workspace` tests with an alias spelling | FR-005 |
| RT-006 | Edits through an inside-root symlink pointing out, a retargeted alias, and a case-variant spelling are dropped; an in-workspace alias spelling is kept | FR-008 |

## 6. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Containment predicates over URIs | Two, with distinct roles: `admits_uri` (lexical pre-filter and advisory flag) and `admits_edit_uri` (write gate); no other copy of either rule |
| SC-002 | Production and test constructors | Test fixtures for real directories go through `from_configured` |
| SC-003 | `tool_surface.json` diff | Only the five `out_of_workspace` descriptions |

## 7. See Also

- [[constitution]] — project principles
- [[MOC-specs]] — all specifications
- [[bridge/008-workspace-root-configured-spelling/spec|workspace-root-configured-spelling]] — which spellings are admitted and how config loading preserves them
- Code: `crates/mcpls-core/src/bridge/workspace_roots.rs`, `crates/mcpls-core/src/bridge/translator/edits.rs`, `crates/mcpls-core/src/bridge/translator/encoding_ctx.rs`, `crates/mcpls-core/src/lib.rs` (diagnostics pump)
