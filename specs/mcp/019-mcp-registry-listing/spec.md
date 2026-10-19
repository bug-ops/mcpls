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
> **Related issues**: #813

> [!abstract]
> mcpls cannot be found through the official MCP server registry. The registry has no `cargo`
> package type, so a listing needs a supported distribution channel (an OCI image and/or an `mcpb`
> bundle) plus a registry manifest whose version the release flow keeps in sync. This spec defines
> what a documented, release-integrated listing path must guarantee. It does not choose
> implementation mechanics.

## 1. Overview

### Problem Statement

Registry clients and MCP-aware IDEs use the official MCP server registry for one-click discovery and
installation. A registry search for mcpls returns nothing. Comparable LSP-to-MCP bridges publish a
registry manifest in their repository and keep its version current on each release, so they are
discoverable where mcpls is not.

Today mcpls ships through `cargo install mcpls`, the `scripts/install.sh` and `scripts/install.ps1`
installers, and GitHub release binaries. None of these is a package channel the registry accepts: the
registry supports `npm`, `pypi`, `nuget`, `oci` and `mcpb` package types only. The repository has a
root `Dockerfile`, but the release workflow neither builds nor publishes an image, and no `mcpb`
bundle is attached to releases. Nothing in the documentation mentions the registry.

### Goal

Every tagged release of mcpls is installable from the official MCP registry through a manifest in the
repository, a publishable artifact the manifest points at, and a release flow that keeps the two in
sync with the released version.

### Out of Scope

- Choosing publishing mechanics (workflow steps, tooling, token handling): plan phase.
- Listing in registries other than the official MCP registry, or in third-party directories.
- Bundling any language server inside the published artifact (see FR-009).
- Changing the tool surface, transports, or configuration model of mcpls.
- Publishing under a package type the registry does not support (a `cargo` entry).
- Retroactively listing releases older than the first listed one.

## 2. User Stories

### US-001: One-click discovery

AS A user of an MCP-aware client or IDE
I WANT to find and install mcpls from the official registry
SO THAT I do not need to know about `cargo install` or the GitHub release page

**Acceptance criteria:**
```
GIVEN a published mcpls release that has been listed
WHEN a registry search is made for mcpls
THEN the result names the release version, the repository, and an installable package
```

### US-002: Version stays current

AS A maintainer
I WANT the registry entry to follow every release without a manual step
SO THAT the listing never advertises a stale or non-existent version

**Acceptance criteria:**
```
GIVEN a release tag vX.Y.Z is pushed
WHEN the release flow completes
THEN the registry shows version X.Y.Z whose package artifact exists and is downloadable
```

### US-003: Safe defaults for registry-installed instances

AS A user installing mcpls from the registry
I WANT the manifest to start mcpls with the same security posture as a manual install
SO THAT one-click install never silently weakens the trust model

**Acceptance criteria:**
```
GIVEN the manifest as committed
WHEN its runtime arguments and environment variables are inspected
THEN none of them enables untrusted-workspace mode or any other trust-relaxing setting
```

## 3. Functional Requirements

### Manifest

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-001 | THE repository SHALL contain one registry manifest (`server.json`) conforming to the registry's published server schema, declaring a reverse-DNS server name under the maintainer's GitHub namespace, the repository, a description, the version, and at least one package entry | must |
| FR-002 | THE manifest SHALL declare the `stdio` transport for every package entry | must |
| FR-003 | THE manifest SHALL declare only package types the registry supports, and SHALL NOT declare a `cargo` package | must |
| FR-004 | THE manifest SHALL declare no environment variable or runtime argument that enables untrusted-workspace mode, nor any config key for it; that mode stays command-line only per the project's security model | must |
| FR-005 | THE manifest SHALL declare no secret, API key or credential input; mcpls needs none for core functionality | must |

### Publishable artifact

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-006 | THE release flow SHALL produce at least one registry-supported artifact for each tagged release: an OCI image built from the repository's `Dockerfile`, an `mcpb` bundle attached to the GitHub release, or both | must |
| FR-007 | WHERE an artifact is an OCI image, THE manifest SHALL reference an immutable version tag (not `latest`), and the image SHALL carry the registry's required ownership label tying it to the manifest name | must |
| FR-008 | WHERE an artifact is an `mcpb` bundle, THE manifest SHALL reference the release-asset URL for that version together with the bundle's SHA-256 digest | must |
| FR-009 | THE artifact SHALL contain the mcpls binary only; language servers are not bundled, and the documentation SHALL say how a registry-installed instance reaches the user's workspace and language servers | must |
| FR-010 | THE artifact SHALL keep the existing runtime posture: unprivileged user, no new network listeners by default, stdio entrypoint | must |

### Release integration

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-011 | THE manifest `version` and every package entry's version SHALL equal the workspace version in the root `Cargo.toml` | must |
| FR-012 | WHEN the manifest version differs from the workspace version THE CI pipeline SHALL fail before merge, and the release flow SHALL fail before publishing anything | must |
| FR-013 | WHEN a tag `vX.Y.Z` is pushed THE release flow SHALL publish the artifact first and the registry entry only after the artifact is available, so the registry never points at a missing artifact | must |
| FR-014 | THE release flow SHALL authenticate to the registry through the repository's GitHub identity (namespace proof via GitHub) without a long-lived secret stored in the repository | should |
| FR-015 | WHEN registry publication fails THE release flow SHALL report it without invalidating the already-published crate, binaries and GitHub release, and the publication SHALL be re-runnable for the same tag | must |
| FR-016 | THE release preparation procedure SHALL include the manifest version bump alongside the other version-bearing files | must |

### Documentation

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-017 | THE user documentation SHALL describe the registry listing, the supported install channel(s), and the maintainer procedure (namespace authentication, publish, re-run) | must |
| FR-018 | THE security documentation SHALL state that registry-installed instances start in the default trust mode and how to run untrusted-workspace mode (command line only) | should |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Security | The registry path adds no way to enable untrusted-workspace mode other than the command line; `unsafe_code = "forbid"` is unchanged and no source change introduces unsafe code |
| NFR-002 | Security | Workflow actions used by the new release steps are pinned to full commit SHAs, per project convention |
| NFR-003 | Supply chain | The published artifact is built from the tagged commit in CI, never from a developer machine |
| NFR-004 | Maintainability | Version consistency is checked by a script or CI step, not by convention; a single source of truth (the workspace version) feeds the manifest check |
| NFR-005 | Simplicity | The listing adds no runtime dependency to `mcpls-core` or `mcpls-cli`; if any crate dependency changes, `fuzz/Cargo.lock` is refreshed per the project rule |
| NFR-006 | Portability | Any new test or script runs on the CI matrix (Linux, macOS, Windows) or is explicitly scoped to a Linux job |

## 5. Data Model

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| Registry manifest | Repository file describing mcpls to the registry | name, description, repository, version, packages, schema reference |
| Package entry | One installable artifact of one release | package type, identifier or URL, version, digest (bundle), transport |
| Release artifact | Image or bundle produced by the release flow | version tag, build commit, ownership label (image), SHA-256 (bundle) |
| Registry namespace | Publisher identity the registry verifies | GitHub owner, reverse-DNS name prefix |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Manifest version differs from `Cargo.toml` | CI fails; release flow aborts before any publish step |
| Image built but registry publish fails | Crate, binaries and GitHub release stay published; the registry step is re-runnable for the same tag |
| Registry publish succeeds but artifact missing | Prevented by FR-013 ordering; if observed, treated as a P1 release defect |
| Pre-release or non-semver tag | [NEEDS CLARIFICATION: are pre-release tags listed, skipped, or rejected by the registry version rules?] |
| Image has no language servers | Documented (FR-009); `mcpls` reports missing servers with the existing install-hint behavior |
| Workspace not visible inside the container | [NEEDS CLARIFICATION: required volume mount and path mapping so paths sent by the client resolve inside the container] |
| Manifest schema version updated by the registry | Schema reference is a maintenance item; CI validates the manifest against the referenced schema |
| Registry rejects the namespace | Release step fails with the registry's message; no partial listing |
| Two package entries (image and bundle) diverge in version | Rejected by FR-011 check |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | A registry search for mcpls after the first listed release | Returns the entry with the release version |
| SC-002 | Releases after the first listed one whose registry version equals the tag, without manual edits | 100% |
| SC-003 | CI run with a deliberately mismatched manifest version | Fails |
| SC-004 | Manifest declarations of untrusted-workspace enablement, secrets or `cargo` packages | 0 |
| SC-005 | A registry-installed instance started per the documentation answers a tool call on a workspace | Succeeds in a documented setup |

## 8. Agent Boundaries

### Always (without asking)

- Keep the manifest version equal to the workspace version in the same change that bumps it.
- Pin any new workflow action to a full commit SHA.
- Run the full pre-commit suite listed in the constitution before committing.

### Ask first

- Choosing between OCI image, `mcpb` bundle, or both as the first channel.
- Adding a registry publishing client or any new third-party action to the release workflow.
- Publishing the first registry entry (a public, hard-to-reverse action).
- Choosing the registry server name and namespace.

### Never

- Add an env var, config key or manifest argument that enables untrusted-workspace mode.
- Commit registry tokens or any credential; publish from a developer machine.
- Introduce `unsafe` code or relax `unsafe_code = "forbid"`.
- Use a mutable tag (`latest`) as the manifest's package reference.
- Place this spec under `.local/specs/`.

## 9. Open Questions

> [!question] Open items
> - [NEEDS CLARIFICATION: which channel first: OCI image, `mcpb` bundle, or both? An OCI image runs in a container that cannot see the host workspace or language servers without mounts; an `mcpb` bundle runs the native binary but needs a per-platform bundle story for the six release targets.]
> - [NEEDS CLARIFICATION: registry server name; the namespace must match the GitHub owner of the repository (confirm owner and exact name).]
> - [NEEDS CLARIFICATION: image registry and visibility for the OCI image (host, repository name, public pull without authentication required by the registry?).]
> - [NEEDS CLARIFICATION: should the image add the language servers most users need, or stay binary-only? FR-009 assumes binary-only.]
> - [NEEDS CLARIFICATION: do pre-release tags get listed?]
> - [NEEDS CLARIFICATION: where the manifest lives (repository root versus a subdirectory) and which tool validates it against the registry schema in CI.]
> - [NEEDS CLARIFICATION: the registry's current schema version and publish flow at implementation time; the field list above reflects the schema as understood at spec time and must be re-verified.]
> - [NEEDS CLARIFICATION: does the registry-installed container need the HTTP transport for any scenario, or is stdio alone enough? FR-002 assumes stdio only.]

## 10. See Also

- [[constitution]]
- [[MOC-specs]]
- [[mcp/001-mcp-tool-surface-and-routing/spec|mcp/001]]
- [[mcp/005-tool-capability-discoverability/spec|mcp/005]]
- [[runtime/003-workspace-supplied-code-execution/spec|runtime/003]]
