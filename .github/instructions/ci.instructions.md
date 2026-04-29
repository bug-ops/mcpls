---
applyTo: ".github/workflows/**,deny.toml,.cargo/**"
---

## Workflow correctness

Feature flag sets in CI jobs must match the pre-commit commands documented in
`CLAUDE.md` exactly. Divergence causes "passes locally, fails CI" failures that are
hard to diagnose — the developer sees green locally and red in CI for the same code.

The rustdoc job must set `RUSTDOCFLAGS="-D warnings"`. Without it, broken intra-doc
links and undocumented public items pass silently in CI while failing for downstream
users who build with warnings-as-errors.

Nightly toolchain is required only for `cargo fmt`. All other jobs must use stable.
Pinning nightly globally causes unnecessary pipeline failures when nightly introduces
a regression unrelated to this project.

## Dependency policy

New dependencies must have their license added to the `deny.toml` allow list in the
same PR. Do not merge a PR that introduces a new crate without verifying that
`cargo deny check licenses` passes. License checks fail fast and block the entire
pipeline.

`cargo deny check advisories` must be clean. Suppress an advisory only with an explicit
`ignore` entry and a comment explaining why the vulnerable code path is not reachable.
