---
applyTo: ".github/workflows/**,deny.toml,.cargo/**"
---

## Feature flag parity

Feature flag sets in CI jobs must match the pre-commit commands in `CLAUDE.md` exactly.
Divergence causes "passes locally, fails CI" failures where neither side is obviously
wrong.

The rustdoc job must set `RUSTDOCFLAGS="-D warnings"`. Without it, broken intra-doc
links and missing `///` on public items pass silently in CI.

Nightly toolchain is required only for `cargo fmt`. All other jobs must use stable to
avoid failures caused by nightly regressions unrelated to this project.

## Dependency and license policy

New dependencies require a license check in the same PR. `cargo deny check licenses`
fails fast and blocks the pipeline — do not merge before it passes.

`cargo deny check advisories` must be clean. Suppress an advisory only with an explicit
`ignore` entry and a comment explaining why the vulnerable code path is unreachable.

When a newer stable `std` API covers functionality provided by a dependency, suggest
removing the dependency and bumping `rust-version` instead. Fewer dependencies reduce
compile time and supply-chain risk. Document the MSRV bump in CHANGELOG.

## MSRV tracking

`rust-version` in `Cargo.toml` is the enforced minimum. The CI matrix must include a
job that builds and tests on exactly that version to catch accidental use of newer APIs.
Without such a job, MSRV guarantees are not verifiable.
