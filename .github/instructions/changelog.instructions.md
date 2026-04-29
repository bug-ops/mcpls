---
applyTo: "CHANGELOG.md"
---

## CHANGELOG review checklist

- Breaking changes to public APIs (new enum variants, removed fields, changed
  signatures) must appear under `### Changed` with an explicit "Breaking change:"
  prefix. A `### Fixed` entry alone is not sufficient.
- New `#[non_exhaustive]` enums or structs must be mentioned: downstream crates
  that match exhaustively need to add a wildcard arm.
- Every entry must reference the PR or issue number in parentheses: `(#NNN)`.
- Entries go under `## [Unreleased]` until a release PR assigns a version.
  Do not add entries directly under a versioned section.
