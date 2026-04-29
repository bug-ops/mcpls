---
applyTo: "crates/mcpls-core/src/config/**,deny.toml,Cargo.toml,crates/*/Cargo.toml"
---

## Configuration

New `#[serde(default)]` fields must have a test that deserialises a config with the
field omitted and asserts the default value. Both JSON and TOML paths should be covered
— `#[serde(rename_all)]` applies uniformly, but TOML and JSON parsers can behave
differently for edge cases like empty strings and null values.

Enum config options must include a round-trip test for every variant covering at least
one serialisation format. A rename or typo in `#[serde(rename_all = "...")]` silently
changes the on-disk format and breaks existing configs.

## Dependencies

When a new crate is introduced, check that its license appears in the `deny.toml` allow
list before merging. `cargo deny check licenses` fails fast in CI and blocks the entire
pipeline. Add the license in the same PR that adds the crate.

`cargo deny check advisories` must be clean. A RUSTSEC advisory in a transitive
dependency is a blocker even when the vulnerable code path is not exercised by this
project.

## MSRV

`rust-version` in `Cargo.toml` is the enforced minimum supported Rust version. Any
stabilised API used in new code must have been available since that version. If a newer
API is needed, bump `rust-version` and document the change in CHANGELOG.
