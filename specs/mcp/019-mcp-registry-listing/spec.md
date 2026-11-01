---
aliases:
  - MCP registry listing
  - Official MCP registry manifest
tags:
  - sdd
  - spec
  - mcp
  - distribution
  - release
  - competitor-gap
created: 2026-10-19
status: draft
related:
  - "[[constitution]]"
  - "[[mcp/001-mcp-tool-surface-and-routing/spec|mcp-tool-surface-and-routing]]"
  - "[[mcp/005-tool-capability-discoverability/spec|tool-capability-discoverability]]"
  - "[[runtime/003-workspace-supplied-code-execution/spec|workspace-supplied-code-execution]]"
---

# Feature: MCP Registry Listing

> [!info] Metadata
> **Type**: research (competitor-gap)
> **Priority**: P4
> **Related issues**: #813, #843

> [!abstract]
> mcpls cannot be found through the official MCP server registry. The registry supports the
> `cargo` package type for crates.io, and mcpls already publishes there, so the crate is the
> registry channel. This spec defines what a documented, release-integrated listing path must
> guarantee: a manifest in the repository, ownership proof in the crate README, version
> consistency enforced by CI and the release flow, and a re-runnable publication step.

## 1. Overview

### Problem Statement

Registry clients and MCP-aware IDEs use the official MCP server registry for discovery and
installation. A registry search for mcpls returns nothing. Comparable LSP-to-MCP bridges publish a
registry manifest in their repository and keep its version current on each release.

mcpls ships through `cargo install mcpls`, the `scripts/install.sh` and `scripts/install.ps1`
installers, and GitHub release binaries. The registry accepts `cargo` packages hosted on crates.io
and verifies ownership through a visible `mcp-name:` token in the crate README. Nothing in the
repository describes mcpls to the registry and no documentation mentions it.

### Goal

Every tagged stable release of mcpls is installable from the official MCP registry through a
manifest in the repository, the crates.io crate it points at, and a release flow that keeps the two
in sync with the released version.

### Out of Scope

- Listing in registries other than the official MCP registry, or in third-party directories.
- OCI image and `mcpb` bundle channels; the `cargo` channel is the only one (decision recorded in
  section 9).
- Bundling any language server (see FR-009).
- Changing the tool surface, transports, or configuration model of mcpls.
- Retroactively listing releases older than the first listed one.

## 2. User Stories

### US-001: One-click discovery

AS A user of an MCP-aware client or IDE
I WANT to find and install mcpls from the official registry
SO THAT I do not need to know about the GitHub release page

**Acceptance criteria:**
```
GIVEN a published mcpls release that has been listed
WHEN a registry search is made for mcpls
THEN the result names the release version, the repository, and the crates.io package
```

### US-002: Version stays current

AS A maintainer
I WANT the registry entry to follow every stable release without a manual step
SO THAT the listing never advertises a stale or non-existent version

**Acceptance criteria:**
```
GIVEN a stable release tag vX.Y.Z is pushed
WHEN the release flow completes
THEN the registry shows version X.Y.Z whose crate exists on crates.io
```

### US-003: Safe defaults for registry-installed instances

AS A user installing mcpls from the registry
I WANT the manifest to start mcpls with the same security posture as a manual install
SO THAT registry install never silently weakens the trust model

**Acceptance criteria:**
```
GIVEN the manifest as committed
WHEN its runtime arguments and environment variables are inspected
THEN none exist, so none enables untrusted-workspace mode or any other trust-relaxing setting
```

## 3. Functional Requirements

### Manifest

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-001 | THE repository SHALL contain one registry manifest (`server.json`) conforming to the registry's published server schema, declaring the name `io.github.bug-ops/mcpls`, the repository, a description of at most 100 characters, the version, and exactly one package entry | must |
| FR-002 | THE manifest SHALL declare the `stdio` transport for every package entry | must |
| FR-003 | THE manifest SHALL declare its package with `registryType: cargo` and identifier `mcpls`, the crate published on crates.io | must |
| FR-004 | THE manifest SHALL declare no environment variable, runtime argument or package argument; in particular none that enables untrusted-workspace mode, and no config key for it, which stays command-line only per the project's security model | must |
| FR-005 | THE manifest SHALL declare no secret, API key or credential input; mcpls needs none for core functionality | must |

### Ownership and artifact

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-006 | THE README packaged with the `mcpls` crate (`crates/mcpls-cli/README.md`) SHALL contain the visible text `mcp-name: io.github.bug-ops/mcpls`, followed by whitespace or an HTML tag. An HTML comment does not work because crates.io strips comments | must |
| FR-007 | THE release flow SHALL publish the crate to crates.io before the registry entry, so the registry never points at a missing package | must |
| FR-008 | THE listing SHALL add no new build artifact, image or secret to the release flow | must |
| FR-009 | THE package SHALL contain the mcpls binary only; language servers are not bundled, and the documentation SHALL say that a registry-installed instance needs `cargo install mcpls` and separately installed language servers | must |
| FR-010 | THE registry-started process SHALL keep the existing runtime posture: stdio entrypoint, no new network listeners by default | must |

### Release integration

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-011 | THE manifest `version` and the package entry's version SHALL equal the workspace version in the root `Cargo.toml` | must |
| FR-012 | WHEN the manifest differs from the workspace version, or declares anything FR-003/FR-004 forbids, THE CI pipeline SHALL fail before merge and the release flow SHALL fail before creating the release | must |
| FR-013 | WHEN a tag `vX.Y.Z` is pushed THE release flow SHALL publish the crate first and the registry entry only after the crate is published | must |
| FR-014 | THE release flow SHALL authenticate to the registry through the repository's GitHub OIDC identity, without a long-lived secret stored in the repository | should |
| FR-015 | WHEN registry publication fails THE release flow SHALL report it without invalidating the already-published crate, binaries and GitHub release, and the job SHALL be re-runnable for the same tag. The registry job is the last job of the workflow | must |
| FR-016 | THE release preparation procedure SHALL include the manifest version bump alongside the other version-bearing files | must |
| FR-017 | THE registry publication job SHALL skip pre-release tags (tags containing `-`) | must |
| FR-018 | THE publishing client SHALL be pinned to a release tag and verified with its Sigstore bundle before it runs | must |
| FR-019 | THE registry publication job SHALL ship disabled until a maintainer confirms the first public publication; enabling it is a deliberate follow-up change | must |

### Documentation

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-020 | THE user documentation SHALL describe the registry listing and its `cargo install` channel, and the contributor documentation SHALL describe the release procedure (version bump, namespace authentication through GitHub OIDC, re-run) | must |
| FR-021 | THE security documentation SHALL state that registry-installed instances start in the default trust mode and that untrusted-workspace mode is command-line only | should |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Security | The registry path adds no way to enable untrusted-workspace mode other than the command line; `unsafe_code = "forbid"` is unchanged and no source change introduces unsafe code |
| NFR-002 | Security | Workflow actions used by the new release steps are pinned to full commit SHAs with a trailing version comment, per project convention |
| NFR-003 | Supply chain | The published crate is built from the tagged commit in CI through crates.io Trusted Publishing, never from a developer machine |
| NFR-004 | Maintainability | Version consistency is checked by `scripts/check-server-json-version.sh` (jq over `cargo metadata`), run in CI and in the release flow; the workspace version is the single source of truth |
| NFR-005 | Simplicity | The listing adds no runtime dependency to `mcpls-core` or `mcpls-cli` and changes no Rust source |
| NFR-006 | Portability | The check script is POSIX `sh`, covered by shellcheck, and runs in a Linux job |

## 5. Data Model

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| Registry manifest | Repository file describing mcpls to the registry | name, description, repository, version, packages, schema reference |
| Package entry | The crates.io crate of one release | registry type `cargo`, identifier, version, transport |
| Ownership token | `mcp-name:` text in the crate README | server name, rendered by crates.io |
| Registry namespace | Publisher identity the registry verifies | GitHub owner, reverse-DNS name prefix |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Manifest version differs from `Cargo.toml` | CI fails; release flow aborts before the release is created |
| Crate published but registry publish fails | Crate, binaries and GitHub release stay published; the registry job is re-runnable for the same tag |
| Pre-release or non-semver tag | The registry job is skipped for tags containing `-` |
| README token missing or glued to trailing characters | The registry rejects publication; FR-006 requires the token on its own line or inside inline code |
| Registry-installed instance has no language servers | Documented (FR-009); mcpls reports missing servers with the existing install-hint behavior |
| Manifest schema version updated by the registry | Maintenance item; the pinned publisher's `validate` command checks the manifest before publishing |
| Registry rejects the namespace | Release step fails with the registry's message; no partial listing |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | A registry search for mcpls after the first listed release | Returns the entry with the release version |
| SC-002 | Stable releases after the first listed one whose registry version equals the tag, without manual edits beyond the version bump | 100% |
| SC-003 | CI run with a deliberately mismatched manifest version | Fails |
| SC-004 | Manifest declarations of untrusted-workspace enablement, secrets, arguments or environment variables | 0 |
| SC-005 | A registry-installed instance started per the documentation answers a tool call on a workspace | Succeeds in a documented setup |

## 8. Agent Boundaries

### Always (without asking)

- Keep the manifest version equal to the workspace version in the same change that bumps it.
- Pin any new workflow action to a full commit SHA.
- Run the full pre-commit suite listed in the constitution before committing.

### Ask first

- Publishing the first registry entry (a public, hard-to-reverse action): remove the `false &&` guard of the `publish-registry` job only after confirmation.
- Adding any further registry channel (OCI image, `mcpb` bundle) or any new third-party action to the release workflow.
- Changing the registry server name or namespace.

### Never

- Add an env var, config key or manifest argument that enables untrusted-workspace mode.
- Commit registry tokens or any credential; publish from a developer machine.
- Introduce `unsafe` code or relax `unsafe_code = "forbid"`.
- Place this spec under `.local/specs/`.

## 9. Decisions and Open Questions

> [!success] Decisions
> - Channel: `cargo` (crates.io). The registry now supports it, so no OCI image, `mcpb` bundle, Dockerfile change or new secret is needed. This replaces the earlier assumption that no `cargo` type exists.
> - Server name: `io.github.bug-ops/mcpls`; the repository owner is `bug-ops`.
> - Manifest location: repository root. CI validation is the jq-based version script; the registry's own `validate` command runs in the publication job.
> - Pre-release tags: skipped.
> - Transport: stdio only.
> - Image host, volume mounts and language-server bundling no longer apply.

> [!question] Open items
> - [NEEDS CLARIFICATION: when to enable `publish-registry` (first public publication); tracked as a follow-up issue.]

## 10. See Also

- [[constitution]]
- [[MOC-specs]]
- [[mcp/001-mcp-tool-surface-and-routing/spec|mcp/001]]
- [[mcp/005-tool-capability-discoverability/spec|mcp/005]]
- [[runtime/003-workspace-supplied-code-execution/spec|runtime/003]]
