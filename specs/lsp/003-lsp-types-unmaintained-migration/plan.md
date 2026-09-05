---
aliases:
  - gen-lsp-types Migration Plan
tags:
  - sdd
  - plan
  - lsp
  - dependency-health
created: 2026-09-05
status: implemented
related:
  - "[[spec]]"
  - "[[constitution]]"
---

# Technical Plan: Migrate `lsp-types` (gluon-lang) to `gen-lsp-types`

> [!info] References
> **Spec**: [[spec]]

> [!important] Why this plan exists as a separate document
> The spec's original research (Sections 1-9) evaluated `ls-types` as the
> migration target and explicitly deferred all implementation mechanics to a
> future `/sdd plan` (spec.md Section 8, "Agent Boundaries → Ask First"). This
> plan captures that mechanics work — retargeted mid-session to `gen-lsp-types`
> after `ls-types` was found archived (see spec.md's Resolution callout) — in
> enough technical depth to be reusable the next time mcpls needs to re-migrate
> an LSP-type dependency (e.g. if `gen-lsp-types` itself is superseded someday).
> It is a record of design decisions actually made, not a forward-looking
> proposal: the migration this plan describes is already implemented (commits
> `8f61f56`, `70a7635`).

> [!important] Resolution of the retargeting decision
> `ls-types 0.0.6` was implemented first, then rejected in review: the
> `tower-lsp-community/ls-types` GitHub repo is archived (last push
> 2026-08-15), its README states it was superseded by `gen-lsp-types`, and
> `tower-lsp-server`'s own main branch had already left it for `gen-lsp-types`
> (only its last *tagged* release, v0.23.0, still pinned `ls-types`). The
> review also found a real silent regression in that first attempt:
> `ls-types`'s `Uri` became strictly-absolute-only, breaking relative-reference
> parsing, and none of ~730 green tests at the time exercised a non-ASCII or
> relative path through `Uri` — the regression was found only by an empirical
> parser probe (building a scratch crate against the removed dependency
> version), not by reading declarations or running the existing suite. This is
> the reason every "verified" claim below is footnoted with *how* it was
> verified (source diff, differential JSON probe, or empirical parser probe),
> not just what was found — the migration's own history shows that declaration
> review alone missed a real regression once already.

## 1. Architecture

### Approach

Single dependency-line swap plus a rename/adaptation sweep, structured as
three ordered phases so behavioral changes are auditable by diff rather than
by developer assertion:

1. **Characterization tests, written and made green against the pre-migration
   `lsp-types 0.97` build**, pinning real JSON wire shapes (fixture strings and
   the serialized *key set*, not just Rust-level round-trip equality) for
   every LSP shape mcpls's bridge layer touches. This is the regression net
   the eventual swap is checked against.
2. **Mechanical sweep** — pure renames requiring no semantic judgment:
   `Uri` construction call sites (~149) and assoc-const → enum-variant
   conversions (~85).
3. **Semantic adaptation** — the rename table below, mixin-nesting fixes,
   `Diagnostic.message` field access, the new `request_typed` binding
   (Section 3), the R6 debug-logging addition, and a scoped
   `#[allow(deprecated)]` for `MarkedString` (still load-bearing on the wire
   for LSP 3.17 hover, so kept rather than dropped to satisfy a lint).

**Invariant across phases 2 and 3:** phase 1's JSON fixtures and expected
outputs must not change — only the Rust type names referencing them may. Any
edit to a fixture string or expected value during phases 2/3 is a behavior
change requiring explicit justification in the PR, not something the sweep
absorbs silently. (In the shipped implementation this invariant was flagged
by review as *unauditable* because the three phases were not landed as
separate commits — see Section 9, Risks — but the invariant itself held per
the final differential JSON probe.)

Dependency line: `lsp-types = { package = "gen-lsp-types", version = "=0.11.0" }`,
no optional features enabled. This is the Cargo package-rename mechanism —
every `use lsp_types::...` import path in the codebase is unaffected; only
`Cargo.toml` names the real crate.

### Component Diagram

```mermaid
graph TD
    OLD["gluon-lang/lsp-types 0.97\n(unmaintained, 2yr+ stale)"] -->|rejected as target| LST["tower-lsp-community/ls-types 0.0.6\n(implemented, then rejected: archived)"]
    LST -->|superseded by| GLT["ribru17/gen-lsp-types 0.11.0\n(actively maintained, adopted)"]
    GLT -->|"package = \"gen-lsp-types\"\nCargo.toml rename"| IMPORT["lsp_types:: import paths\n(unchanged throughout codebase)"]
    IMPORT --> RT["LspClient::request_typed&lt;R: lsp_types::Request&gt;\n(binds method string + result type to one impl)"]
    RT --> SITES["15 typed call sites\nbridge/translator/*.rs"]
```

### Key Design Decisions

| Decision | Choice | Rationale | Alternatives Considered |
|----------|--------|-----------|--------------------------|
| Migration target | `gen-lsp-types 0.11.0`, not `ls-types` (spec's original target) | `ls-types` is archived/superseded; `gen-lsp-types` is actively maintained, not archived, and is what `tower-lsp-server`'s own main branch already depends on | Stay on `gluon-lang/lsp-types` (rejected: the original supply-chain-risk finding stands); land `ls-types` as a deliberate interim step (rejected by the reviewing critic: would close #297 on a target already superseded, and required documenting the finding as accepted risk rather than a fix) |
| Method/result-type binding | New `LspClient::request_typed<R: lsp_types::Request>` helper, deriving both the JSON-RPC method string (`R::METHOD.as_str()`) and the expected result type (`R::Result`) from one trait impl | Closes the exact bug class that produced the plan's own C1 finding (a hand-transcribed rename table mapped `GotoDefinitionResponse` to the wrong type, `Definition` instead of `DefinitionResponse` — a runtime failure with zero compile error under the untyped `request` API). A net deletion of a magic string *and* a hand-picked type per call site, not a new abstraction | Keep the untyped `request(method: &str, ...)` and hand-transcribe a full rename table (rejected: exactly the mechanism that produced C1); add only method-string binding without result-type binding (rejected: leaves the more dangerous half of the bug class open) |
| `textDocument/diagnostic` Partial-response handling | Left on the untyped `request` path with hand-rolled `Report \| Partial` union handling, not bound via `request_typed` | `DocumentDiagnosticRequest::Result` in `gen-lsp-types` is the non-nullable `DocumentDiagnosticReport` alone — the partial-results shape is split off into a separate associated type (`RequestWithPartialResults::PartialResult`). Binding this one site to `R::Result` would silently drop the `Partial` arm mcpls handles today (degrade-to-empty-list on a partial response) | Bind to `R::Result` and accept the dropped `Partial` arm as a documented behavior change (rejected: unlike the three accepted behavior changes in spec.md's Resolution, this one had a low-cost alternative — hand-rolled union handling — so there was no reason to accept the regression) |
| `MarkedString` deprecation | Scoped `#[allow(deprecated)]` on the two hover-conversion functions only | `Hover.contents` in `gen-lsp-types` is deprecated-but-still-the-wire-format for LSP 3.17 legacy hover shapes; dropping support to satisfy a lint would silently discard hover text from any server still sending that shape | Rewrite hover handling to `MarkupContent`-only (rejected: user-visible regression traded for a clean build); crate-wide `#[allow(deprecated)]` (rejected: too broad, would hide future unrelated deprecations) |
| Fixture/test strategy | Characterization tests written and green against the *old* dependency first, asserting serialized JSON key sets (not just round-trip equality), covering every variant of the 7 union enums mcpls consumes (`CodeActionResponse`, `CompletionResponse`, `DocumentDiagnosticReport`, `DocumentSymbolResponse`, `Documentation`, `HoverContents`/`Contents`, `TextDocumentContentChangeEvent`) | Untagged-enum wire shapes resolve in declaration order; `gen-lsp-types` reordered several of these relative to `gluon-lang/lsp-types` (benign here because the variants are disjoint, but only verified by exhaustive fixture coverage, not by reading declarations one at a time) | Round-trip equality assertions only (rejected: pins Rust-level equality, not the LSP-3.17 wire pin the migration must not silently drift from — e.g. accidentally serializing a 3.18-only field); reviewing declarations without fixtures (rejected: this is exactly how the union-enum reordering hazard would go unnoticed) |

## 2. Rename / Adaptation Reference

Durable reference for a future similar migration — not exhaustive, the
high-hazard rows only.

| Old (`gluon-lang/lsp-types 0.97`) | New (`gen-lsp-types 0.11.0`) | Hazard class |
|---|---|---|
| `GotoDefinitionResponse::Scalar(Location)` | `DefinitionResponse::Definition(Definition::Location(..))` | **Critical** — first-draft plan mapped this to the wrong top-level type (`Definition` instead of `DefinitionResponse`); see C1 in Section 4 |
| `GotoDefinitionResponse::Array(Vec<Location>)` | `DefinitionResponse::Definition(Definition::LocationList(..))` | same as above |
| `GotoDefinitionResponse::Link(Vec<LocationLink>)` | `DefinitionResponse::DefinitionLinkList(..)` | same as above — this is the arm mcpls actually receives, since it advertises `link_support: Some(true)` |
| `DocumentChanges::Edits \| Operations` (nested in `WorkspaceEdit.document_changes`) | `Option<Vec<DocumentChange>>`, `DocumentChange = TextDocumentEdit \| CreateFile \| RenameFile \| DeleteFile` | Medium — the two-arm match collapses into one iteration keeping `TextDocumentEdit` and discarding the rest (matches existing behavior); **name collision**: mcpls's own DTO also defines `DocumentChanges` in the same file, so every `lsp_types::` path in this block must be explicitly qualified or it silently resolves to the wrong type |
| `HoverContents::{Scalar, Array, Markup}` | `Contents::{MarkedStringList, MarkedString, MarkupContent}` (reordered) | Low here (disjoint shapes), but the general hazard — untagged-enum declaration order is load-bearing for serde — applies to all 7 union enums mcpls touches; addressed by the exhaustive fixture strategy in Section 1, not by per-type review |
| `ImplementationRequest`/`TypeDefinitionRequest` responses | Bound via `request_typed::<ImplementationRequest>`/`request_typed::<TypeDefinitionRequest>` — **not** transcribed by hand | Deliberately *not* given a rename-table row: the lesson from the `GotoDefinitionResponse` bug was to let the compiler name these types via `R::Result`, not to hand-transcribe a second row the same way |

## 3. API Design

```rust
// crates/mcpls-core/src/lsp/client.rs

impl LspClient {
    /// Sends a typed LSP request, deriving both the JSON-RPC method name and
    /// the expected result type from a single `lsp_types::Request` impl, so a
    /// call site cannot pair a mismatched method string with the wrong result
    /// type (the class of bug this migration's plan review caught in its
    /// first draft).
    pub async fn request_typed<R: lsp_types::Request>(
        &self,
        params: R::Params,
        timeout_duration: Duration,
    ) -> Result<R::Result> {
        self.request(R::METHOD.as_str(), params, timeout_duration).await
    }
}
```

Applied at 15 call sites across `bridge/translator/{navigation,edits,symbols,
call_hierarchy,assist}.rs`. `lifecycle.rs` (`initialize`) and `shutdown` stay
on the untyped `request` — their params/shutdown-null shape does not benefit
from this binding — as does the one `textDocument/diagnostic` site (Section 1
key decision table).

No new MCP tool, HTTP endpoint, or public config surface. The only "API"
changes are internal: `LspClient::request_typed` (new, `pub` within the
crate's LSP layer) and the `lsp_types::` symbol renames themselves, which are
not part of mcpls's own public API surface.

## 4. Review Findings Log

Kept for provenance — this is what "two full critic-review rounds" (spec.md
Resolution callout) actually found, condensed:

| ID | Round | Finding | Resolution |
|----|-------|---------|------------|
| C1 | 1st critic pass on 1st plan draft | `GotoDefinitionResponse` mapped to wrong type (`Definition` instead of `DefinitionResponse`) — silent runtime break, zero compile error | Fixed in plan revision; generalized into the `request_typed` mechanism (Section 3) so the same class cannot recur |
| N1 | 2nd critic pass on revised plan | `textDocument/diagnostic`'s `Partial` response shape would be dropped if bound via `request_typed` | Left on untyped `request` path with hand-rolled union handling (Section 1 decision table) |
| S1 | Post-implementation validation | `workspace/symbol` fabricated a `(1,1)` placeholder range for range-less server responses — pre-migration this shape failed to deserialize entirely | Fixed: such entries are now dropped instead of fabricating a location (spec.md Resolution, behavior change 2) |
| S2 | Post-implementation validation | `rename` could deserialize an LSP-3.18 snippet-shaped edit and pass its literal `${1:name}` placeholder syntax through as plain text — pre-migration this failed deserialization with a hard error | Fixed: such edits are now dropped, consistent with `CreateFile`/`RenameFile`/`DeleteFile` already being dropped in the same conversion (spec.md Resolution, behavior change 3) |
| M1 (Windows) | 2nd critic pass | A proposed `try_path_to_uri` guard (`!path.is_absolute()`) would silently narrow Windows behavior, since `\foo` satisfies `has_root()` but not `is_absolute()` | Rejected in favor of matching each platform's existing `file_url` contract; a rooted-but-not-absolute Windows test was added instead |

## 5. Testing Strategy

| Level | What | Verification method | Outcome |
|-------|------|----------------------|---------|
| Characterization | JSON fixtures for every union-enum variant mcpls consumes, `Uri` round-trips (non-ASCII, spaced, relative, reserved-char paths), `WorkspaceEdit.document_changes` in both old-shape forms, hover in all 3 variants | Written and green against `lsp-types 0.97` first; only Rust type names may change in later phases, not fixture content | Landed; two tautological `Uri::from(s).as_ref() == s` tests were later found and replaced with real round-trips through `try_path_to_uri`/`uri_to_path` |
| Differential probe | `initialize` params, `LocationLink[]` response, diagnostics `Partial`/`Report` shapes, `SymbolKind` Debug formatting, unknown-enum-value handling | Two scratch crates built against each dependency version, byte-diffed | `initialize` params byte-identical; all other probed shapes matched or were the deliberate documented differences |
| New-arm coverage | `WorkspaceSymbolLocation::LocationUriOnly` (drop), `Edit::SnippetTextEdit` (drop), `DocumentDiagnosticReportResult::Partial` (degrade-to-empty), malformed call-hierarchy URI (`Error::FileIo`) | Handler-path tests (not unit-level), added during the post-implementation fix pass | All 4 previously-untested wire-reachable arms now covered |
| Full suite | `cargo nextest run --workspace --all-features --lib --bins` | Run pre- and post-rebase onto main | 758/758 passing |
| Docs/lint | `cargo +nightly fmt --check`, `cargo clippy --all-targets --all-features --workspace -- -D warnings`, `cargo test --doc`, rustdoc gate | Run pre- and post-rebase | All green |

## 6. Security

Full audit performed as a dedicated pass (see spec.md Resolution callout for
the summary). Key points not restated there:

- Net dependency change: `+gen-lsp-types`, `-lsp-types`, `-fluent-uri`,
  `-bitflags 1.3.2`, `-serde_repr` — a reduction, zero new transitive
  dependencies.
- `gen-lsp-types` has zero `unsafe`, no build script, no filesystem/network/
  process access — pure serde type definitions. crates.io provenance
  (`.cargo_vcs_info.json` sha) matches the GitHub tag exactly.
- The one flagged behavior change relevant to security (`Uri` losing its
  validating parse) was proven empirically **not** a weakening: the removed
  `fluent-uri` check was a character-class syntax check only, verified via a
  scratch probe against the actual removed parser version, and never blocked
  path traversal, percent-encoded traversal, or authority confusion. The real
  defenses (`validate_path_against_roots`'s `canonicalize()` + root
  containment, and `uri_to_path`'s strict `url::Url::parse`) are unchanged and
  run before any filesystem access, independent of `lsp_types::Uri`.
- `cargo audit`: 0 vulnerabilities, 0 warnings across 221 crates. `cargo deny
  check`: licenses/advisories/bans/sources all clean, `deny.toml` unchanged.

## 7. Rollout Plan

As implemented (two commits, no feature flag):

1. `8f61f56` — `build(deps): migrate lsp-types to gen-lsp-types 0.11.0`: the
   dependency swap plus the full mechanical/semantic adaptation sweep
   (Section 1, phases 2-3) and the `request_typed` mechanism (Section 3).
2. `70a7635` — `fix(bridge): update rename test call site for Position struct
   API`: a follow-up test-only fix surfaced by the rebase onto main (the
   `Position { line, character }` struct API predates this migration; the
   rebase brought the two changes into contact).
3. No migration/rollback mechanism needed beyond a normal revert — the change
   is contained to `Cargo.toml`/`Cargo.lock` and internal type usage, with no
   persisted data format or external API affected.

**Deviation from the plan as designed:** the three-phase commit split
(Section 1's audit invariant) was not landed as three separate commits —
phase 1's characterization tests, the mechanical sweep, and the semantic
adaptation all arrived in `8f61f56` as one commit. Post-implementation review
flagged this as making the invariant unauditable by diff (a reviewer cannot
mechanically confirm the fixtures didn't change); the differential JSON probe
in Section 5 was the mitigation actually used to gain equivalent confidence
after the fact. A future similar migration should land the characterization
commit separately if this auditability property is wanted from the start.

## 8. Risks and Mitigations

| Risk | Impact | Probability | Mitigation |
|------|--------|--------------|-------------|
| A future rename-table row is hand-transcribed incorrectly the same way C1 was | high (silent runtime break, zero compile error) | low (mitigated by design) | `request_typed` removes the hand-transcription step entirely for anything bound to a `Request::Result`; only truly bespoke enum mappings (like the `Definition`/`DefinitionResponse` inner variants) still require manual review |
| `gen-lsp-types` itself becomes unmaintained or archived (the same fate as `ls-types`) | medium | unknown | The Cargo package-rename mechanism (Section 1) means a future re-migration to a third crate would not require an import-path rewrite either — this plan's Section 2 reference table format is reusable for that migration |
| The un-split single-commit landing (Section 7 deviation) makes a future bisect of a regression harder | low | low | The differential JSON probe artifacts and this plan's Review Findings Log (Section 4) exist as an alternate audit trail if a regression surfaces later |

## See Also

- [[spec]] — feature specification, including the Resolution callout and
  resolved Open Questions
- [[MOC-specs]] — all specifications
- `crates/mcpls-core/src/lsp/client.rs` — `LspClient::request_typed`
- `crates/mcpls-core/src/bridge/translator/` — the 15 `request_typed` call
  sites and the semantic-adaptation changes (symbols.rs, edits.rs,
  diagnostics.rs, call_hierarchy.rs)
- `crates/mcpls-core/src/bridge/encoding.rs` — critical position-encoding
  path, confirmed untouched by this migration
- `CHANGELOG.md` — `[Unreleased]` entry with the exact shipped wording for
  the three user-visible behavior changes
