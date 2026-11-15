---
aliases:
  - Release and CI gates
  - Release bump check
tags:
  - sdd
  - spec
  - testing
  - ci
  - release
created: 2026-11-15
status: implemented
related:
  - "[[constitution]]"
---

# Feature: Release version and CI hygiene gates

> [!info] Metadata
> **Type**: enhancement (retroactive)
> **Priority**: P3
> **Issues**: #905, #904 (shipped in #908, #923, #920)

## 1. Overview

### Problem Statement

A release tag could carry breaking changes under a Cargo-compatible version, public API breaks were
not checked against the published crate, and tracing messages and lint suppressions drifted.

### Goal

CI and the release workflow fail on those defects before a tag is published.

### Out of Scope

- Changing what counts as a breaking change in the changelog or commit conventions.

## 2. Functional Requirements

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-001 | THE release workflow SHALL run `scripts/check-release-bump.sh <version>` in a read-only `bump-check` job (full history) and `create-release` SHALL depend on it | must |
| FR-002 | THE script SHALL fail when the tag version is Cargo-compatible with the previous stable tag (same leftmost non-zero component; `0.0.x` is never compatible) and the release has breaking evidence: a `**Breaking:**` line in the version's changelog section, a `!` Conventional Commits subject, or a `BREAKING CHANGE:` footer between the base and the tag. It SHALL also fail when the changelog has no `## [<version>]` section, exit 2 on a malformed version or arguments, and skip pre-release tags as a base | must |
| FR-003 | THE release workflow SHALL run `cargo semver-checks check-release -p mcpls-core` in a separate read-only `semver-check` job (it builds crates.io code) with a pinned toolchain and a pinned tool version, and `create-release` SHALL depend on it | must |
| FR-004 | THE script SHALL have a self-test, `scripts/test-check-release-bump.sh`, run in CI and shellchecked | must |
| FR-005 | CI SHALL run `scripts/check-tracing-messages.py`, which rejects a production `trace!`/`debug!`/`info!`/`warn!`/`error!`/`event!` whose message is not a constant literal (values belong in structured fields; test modules are exempt), and `scripts/test-check-tracing-messages.sh` as its self-test | must |
| FR-006 | LINT suppressions SHALL be `#[expect(.., reason = ..)]`, with `clippy::allow_attributes` enabled so a stale suppression fails the build, and the workspace SHALL deny the cargo lint `unused_dependencies` | must |

## 3. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Security | Jobs that build or run third-party code have no write token; GitHub Actions are pinned to a commit SHA |
| NFR-002 | Portability | The scripts are POSIX `sh` (the tracing checker is Python 3) |

## 4. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| First release (no base tag) | Not compared; passes |
| Tag `0.9.1` to `0.9.2` with a `feat!` commit | Fails, naming the commits |
| Tag `0.9.1` to `0.10.0` with breaking commits | Passes |

## 5. See Also

- [[constitution]] — project principles
- [[MOC-specs]] — all specifications
- Code: `.github/workflows/release.yml`, `.github/workflows/ci.yml`, `scripts/check-release-bump.sh`, `scripts/check-tracing-messages.py`
