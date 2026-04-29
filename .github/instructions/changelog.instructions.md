---
applyTo: "CHANGELOG.md"
---

## Changelog format

Breaking changes to public APIs must appear under `### Changed` with an explicit
"Breaking change:" prefix. A `### Fixed` entry alone is not sufficient — downstream
users scanning the changelog for breakage will miss it.

Additions of `#[non_exhaustive]` to existing public enums or structs are breaking
changes for crates that match exhaustively. Document the required migration (add a
wildcard arm, update struct initialisation) in the entry.

Every entry must reference the PR or issue number: `(#NNN)`.

New entries go under `## [Unreleased]`. Do not add entries directly under a versioned
section — version assignment happens in the release PR.
