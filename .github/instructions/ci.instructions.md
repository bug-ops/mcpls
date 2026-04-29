---
applyTo: ".github/workflows/**,deny.toml,.cargo/**"
---

## CI and toolchain review checklist

### Workflows

- Feature flag sets in CI jobs must match the local pre-commit commands in `CLAUDE.md`
  exactly: `--all-features` for clippy and nextest, `--no-deps --all-features` for
  rustdoc. Divergence causes "passes locally, fails CI" issues.
- `RUSTDOCFLAGS="-D warnings"` must be set in the rustdoc job. Without it, broken
  intra-doc links and missing doc comments pass silently.
- Nightly toolchain is required only for `cargo +nightly fmt`. All other jobs must use
  stable. Pinning nightly globally causes unnecessary breakage on nightly regressions.

### deny.toml

- New dependencies require a license check before merge. Add the license to the
  `allow` list in the same PR that introduces the crate. Do not merge a PR that
  introduces a new crate without verifying `cargo deny check licenses` passes.
- Known unlisted licenses that will fail CI: `CC0-1.0` (used by `notify`).
- `cargo deny check advisories` must be clean. A new RUSTSEC advisory in a transitive
  dependency is a blocker even if the vulnerable code path is not exercised.

### MSRV

- `rust-version` in `Cargo.toml` is the enforced minimum. Any API stabilised after
  that version requires either a version bump (document in CHANGELOG) or a conditional
  compile guard.
