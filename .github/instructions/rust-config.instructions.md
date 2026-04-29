---
applyTo: "crates/mcpls-core/src/config/**,deny.toml,Cargo.toml,crates/*/Cargo.toml"
---

## Config and dependency review checklist

### deny.toml

- When a new crate is introduced, verify its license appears in the `allow` list.
  `cargo deny check licenses` runs in CI and will fail on first run if the license is
  missing. Known gap: `CC0-1.0` (used by `notify`).
- Check `cargo deny check advisories` output for any new RUSTSEC advisories in added
  dependencies.

### Config (`config/mod.rs`)

- New `#[serde(default)]` fields on public config structs must have a test covering
  the omitted-key case (TOML round-trip, not just JSON).
- Enum config options must include serde round-trip tests for every variant, both JSON
  and TOML, since `#[serde(rename_all)]` applies to both serializers independently.
- LSP server discovery heuristics (file-pattern matching) must be case-insensitive on
  the extension comparison — APFS and NTFS are case-insensitive filesystems.
