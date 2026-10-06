---
aliases:
  - Unsupported file_patterns forms
  - file_patterns silently ignored
tags:
  - sdd
  - spec
  - bug
  - config
  - diagnosability
created: 2026-10-06
status: draft
related:
  - "[[constitution]]"
  - "[[config/001-config-discovery-and-heuristics/spec|config/001-config-discovery-and-heuristics]]"
  - "[[config/002-typescript-7-native-server-support/spec|config/002-typescript-7-native-server-support]]"
  - "[[mcp/005-tool-capability-discoverability/spec|mcp/005-tool-capability-discoverability]]"
---

# Feature: Reject or report `file_patterns` forms that cannot be mapped to an extension

> [!info] Metadata
> **Type**: bug
> **Priority**: P3
> **Author**: Andrei G.
> **Source**: continuous-improvement cycle, live-test finding
> **Issue**: #669

> [!abstract]
> `file_patterns` is documented as a glob, but mcpls only reads one thing from it: a plain trailing
> extension such as `**/*.rs`. Any other form is dropped without a word. A config with
> `file_patterns = ["**/*.{cpp,h}"]` loads cleanly, and the first `get_hover` on a `.cpp` file fails
> with `no LSP server configured for language: plaintext`, which names neither the cause nor the
> setting. This spec makes an unmappable pattern visible at load time and makes the resulting
> routing error name what is missing.

## 1. Overview

### Problem Statement

`LspServerConfig::file_patterns` has exactly one consumer: `ServerConfig::build_effective_extension_map`
(`crates/mcpls-core/src/config/mod.rs`) calls `extract_extension_from_pattern` for each pattern and
inserts `extension -> language_id` into the extension map that language detection uses. The
extractor takes the part of the last path segment after its last dot and accepts it only when it is
made of ASCII alphanumerics, `_` or `-`. Everything else returns `None`, and the caller skips it
silently.

Forms that are ignored without notice:

| Form | Example | Why it is dropped |
|------|---------|-------------------|
| Brace expansion | `**/*.{cpp,h}` | The extension token contains `{`, `,` and `}` |
| No extension | `Makefile`, `src/**`, `**/*` | No trailing dot-extension |
| Character class | `**/*.[ch]` | The token contains `[` and `]` |
| Single-character wildcard | `**/*.ts?` | The token contains `?` |
| Dotfile | `.eslintrc` | The basename starts with a dot |

**Reproduced.** A config with `language_id = "cpp"`, `command = "clangd"` and
`file_patterns = ["**/*.{cpp,h}"]` loads without a warning. `get_tool_support` reports coverage
`all` for `get_hover`, because coverage is judged from server capabilities, not from routing. A
`get_hover` on a `.cpp` file then fails: language detection finds no mapping for `cpp`, falls back
to `plaintext`, and the caller sees the cryptic `no LSP server configured for language: plaintext`.

**Documentation disagrees with the code.** `docs/user-guide/configuration.md` lists the glob syntax
`**`, `*`, `?` and `[abc]` as supported, while `docs/user-guide/getting-started.md` and
`docs/user-guide/installation.md` show only plain `**/*.ext` patterns. A user who follows the
configuration page writes a pattern the code ignores.

**Quirk found while reading the extractor.** A pattern that names one file, such as `src/main.rs` or
`Cargo.toml`, is accepted and registers the whole extension (`rs`, `toml`), so it widens the match
beyond what the pattern says. This is the opposite failure (over-match) and is recorded here so the
decision on supported forms covers it.

### Goal

A `file_patterns` entry that mcpls cannot map to an extension never disappears silently: it is
rejected at config load, or at least reported with the server entry, the pattern and the supported
forms. When routing then fails for a file whose extension has no mapping, the error says which
extension is unmapped and which patterns are configured.

### Out of Scope

- Full glob matching of file paths (matching files by pattern instead of by extension).
- Per-directory routing of a language to different servers.
- Changing the built-in extension table (`default_language_extensions`) or
  `workspace.language_extensions`.
- Correcting the `get_tool_support` coverage value for an unrouted extension (recorded in section 9).
- Technical design: recorded in a plan after this spec is approved.

## 2. User Stories

### US-001: A broken pattern is reported when the config loads

AS A user writing `mcpls.toml`
I WANT mcpls to tell me when a `file_patterns` entry has no effect
SO THAT I find the mistake at startup, not on the first failed tool call.

**Acceptance criteria:**
```
GIVEN a config with an lsp_servers entry for clangd and file_patterns = ["**/*.{cpp,h}"]
WHEN the config is loaded
THEN the load fails (or logs a warning, as decided) naming the entry, the pattern and the supported forms
  AND the message is produced without starting any server
```

### US-002: A routing failure explains itself

AS A user whose file is not routed to any server
I WANT the error to name the file extension and the configured patterns
SO THAT I can see why `.cpp` did not reach my clangd entry.

**Acceptance criteria:**
```
GIVEN no mapping for the extension cpp
WHEN get_hover is called on a .cpp file
THEN the error names the extension "cpp"
  AND lists the file_patterns configured across servers
  AND does not present "plaintext" as the only explanation
```

### US-003: Supported forms keep working

AS A user with plain `**/*.ext` patterns
I WANT nothing to change
SO THAT existing configs keep loading and routing.

**Acceptance criteria:**
```
GIVEN the built-in default config and every pattern shown in the user guides
WHEN the config is loaded
THEN it loads without error or warning and routes as before
```

### US-004: The documentation states what is supported

AS A user reading the configuration guide
I WANT the supported pattern forms stated exactly
SO THAT I do not write a pattern the code ignores.

**Acceptance criteria:**
```
GIVEN the user guide pages that describe file_patterns
WHEN they are read
THEN they list the supported form, say how to cover several extensions, and agree with each other and with the code
```

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-001 | THE SYSTEM SHALL define the supported `file_patterns` forms in one place: a final path segment of the form `*.EXT`, where `EXT` is a non-empty run of ASCII letters, digits, `_` and `-`, optionally preceded by any directory part such as `**/` | must |
| FR-002 | WHEN a configured `file_patterns` entry is not a supported form THE SYSTEM SHALL NOT drop it silently | must |
| FR-003 | WHEN a configured entry is not a supported form THE SYSTEM SHALL reject the config at load with `Error::InvalidConfig` naming the server entry (its id), the pattern and the supported forms, unless the decision in section 9 selects a warning | must |
| FR-004 | WHERE the decision is a warning THE SYSTEM SHALL log it at `WARN` once per entry and pattern at load, naming the entry, the pattern and the supported forms | must |
| FR-005 | THE check SHALL run in `ServerConfig::validate`, so every load path (file, default, programmatic construction) is covered by it | must |
| FR-006 | WHEN `NoServerForLanguage` is returned for a file whose language fell back to `plaintext` THE SYSTEM SHALL include in the error the file extension and the configured `file_patterns` | must |
| FR-007 | WHEN the fallback language is not `plaintext` THE existing error text SHALL be unchanged | should |
| FR-008 | THE SYSTEM SHALL NOT change routing for supported patterns, including the default config and every pattern in the user guides | must |
| FR-009 | THE configuration guide SHALL state the supported forms, remove the claim of full glob syntax, and show how to cover several extensions with several patterns (for example `["**/*.cpp", "**/*.h"]`) | must |
| FR-010 | THE getting-started, installation and configuration guides SHALL agree on the supported forms | must |
| FR-011 | THE SYSTEM SHALL decide how a pattern that names a single file (`src/main.rs`, `Cargo.toml`) is treated: kept as an extension pattern with a documented caveat, or rejected as an unsupported form | should |
| FR-012 | THE SYSTEM MAY support brace expansion (`**/*.{cpp,h}`) as an extension of FR-001; it is an option to evaluate, not a requirement of this spec | may |
| FR-013 | THE `.local/testing/` playbooks SHALL gain a case for each unsupported form and for the no-server error, and the coverage status for config SHALL be reset | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Diagnosability | Every message names the specific field and value that failed, as [[config/001-config-discovery-and-heuristics/spec\|config/001]] NFR-005 already requires of `Error::InvalidConfig` |
| NFR-002 | Type safety | A supported pattern is parsed once into a typed value (an extension newtype), and the extractor, the validator and the error text use that one parse, per [[constitution]] |
| NFR-003 | Consistency | The validator and the extension-map builder cannot disagree: a pattern accepted by one is accepted by the other, with no second copy of the rule |
| NFR-004 | Compatibility | Pre-1.0 compatibility is not a constraint; if load-time rejection is chosen, a config that loaded before and is now rejected is recorded in `CHANGELOG.md` as a breaking change |
| NFR-005 | Security | Error and log text includes only user-authored config text (pattern, entry id), never server-supplied text |
| NFR-006 | Startup cost | The check is string inspection on already-loaded config; no filesystem access |

## 5. Data Model

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| `file_patterns` entry | A configured string on an `lsp_servers` entry | raw pattern, owning server id |
| Supported pattern (new, typed) | A pattern that maps to exactly one extension | extension (validated token) |
| Unsupported pattern report (new) | What the validator or the warning carries | server id, pattern, supported-forms text |
| Extension map (existing) | `extension -> language_id` used by language detection | built from workspace mappings overlaid with supported patterns |
| `NoServerForLanguage` (existing) | Routing error | language id; gains extension and configured patterns when the language is the `plaintext` fallback |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| `**/*.{cpp,h}` | Reported at load (FR-002, FR-003 or FR-004); with brace expansion adopted (FR-012) it maps both `cpp` and `h` |
| `Makefile` | Reported: no extension, so no mapping; the message points to `workspace.language_extensions` as the way to map extensionless files, if that mechanism supports it (`[NEEDS CLARIFICATION: confirm]`) |
| `src/**` or `**/*` | Reported: no extension |
| `**/*.[ch]` | Reported: character class |
| `**/*.ts?` | Reported: single-character wildcard |
| `.eslintrc` or other dotfile | Reported |
| `**/*.` (empty extension) | Reported |
| `foo.tar.gz` | Supported as extension `gz` today; stays, with the documented rule that the last segment after the final dot is the extension |
| `src/main.rs` (names one file) | Decided under FR-011; today it registers `rs` for the whole server |
| A server entry with an empty `file_patterns` | Valid and unchanged: the entry relies on the built-in or `workspace.language_extensions` mappings |
| Unsupported pattern on an entry whose extension is also mapped through `workspace.language_extensions` | Still reported, because the pattern has no effect; severity follows the decision (a warning is enough here, `[NEEDS CLARIFICATION: confirm]`) |
| Two entries claim the same extension | Unchanged: the existing duplicate-claim rule and map overlay order stand |
| File with an unmapped extension and no patterns configured at all | `NoServerForLanguage` names the extension and says no patterns are configured |
| Windows path separators in a pattern (`**\\*.rs`) | Out of scope; `[NEEDS CLARIFICATION: is the split on `/` only a defect on Windows?]` |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Config with `file_patterns = ["**/*.{cpp,h}"]` | Load rejects (or warns once) naming entry, pattern and supported forms; before the change: silent |
| SC-002 | The same config, `get_hover` on `.cpp` when it was allowed to load | Error names `cpp` and the configured patterns |
| SC-003 | Table of unsupported forms (section 1) | Each is reported; unit test covers every row |
| SC-004 | Built-in default config and every pattern in the three user guides | No error, no warning, identical routing |
| SC-005 | Documentation | The three guides agree; the claim of `?` and `[abc]` support is gone or the forms are supported |
| SC-006 | Validator and extension-map builder | One shared parse; a test asserts they accept the same set |
| SC-007 | `CHANGELOG.md` | Entry with the PR link, marked breaking if rejection is chosen |

## 8. Agent Boundaries

### Always (without asking)
- Reproduce with the clangd config before changing behavior, and keep it as a regression case under `.local/testing/`.
- Parse the pattern once into a typed value and reuse it in the validator, the map builder and the error text.
- Keep the built-in default config and the documented plain patterns valid.
- Run the full pre-commit suite and update `CHANGELOG.md` with the PR link.

### Ask First
- Choosing rejection over a warning (section 9), since it changes which configs load.
- Adding brace expansion or any glob matching (FR-012).
- Adding a dependency for glob parsing (constitution VII).
- Rejecting single-file patterns (FR-011).

### Never
- Leave an unmappable pattern silently ignored.
- Add a second implementation of the extension rule next to the extractor.
- Include server-supplied text in the new messages.
- Change routing for supported patterns.

## 9. Open Questions

- [NEEDS CLARIFICATION: reject at load, or warn? Rejection matches the rule that invalid config fails with a diagnosable `Error::InvalidConfig` ([[config/001-config-discovery-and-heuristics/spec|config/001]] FR-006) and is correct pre-1.0, but it blocks startup for a config where the pattern is merely redundant (the extension is also mapped elsewhere). Recommended default: reject, with the message naming the fix; revisit if redundant patterns turn out to be common.]
- [NEEDS CLARIFICATION: brace expansion. Supporting `{a,b}` removes the most likely mistake and needs only a bounded expansion of one brace group; character classes and `?` have no sensible extension mapping and should stay unsupported. Recommended default: not in this change; list several patterns instead, and revisit on demand.]
- [NEEDS CLARIFICATION: single-file patterns (`src/main.rs`) currently over-match the whole extension (FR-011). Reject, or keep with a documented caveat? Recommended default: keep and document, since rejecting could break working configs for no routing benefit.]
- [NEEDS CLARIFICATION: can an extensionless file (`Makefile`, `Dockerfile`) be mapped at all today? `workspace.language_extensions` is keyed by extension; if there is no filename mapping, the message cannot offer a fix and the gap is a separate enhancement.]
- [NEEDS CLARIFICATION: `get_tool_support` reports coverage `all` for a language whose extension has no mapping. Should coverage take the extension map into account, or is that a separate issue? Recommended default: separate issue, referenced from this spec.]
- [NEEDS CLARIFICATION: is splitting on `/` only a defect for Windows-style patterns?]

## 10. See Also

- [[constitution]] — project principles
- [[MOC-specs]] — all specifications
- [[config/001-config-discovery-and-heuristics/spec|config/001]] — config loading, validation and the `Error::InvalidConfig` rules this extends
- [[config/002-typescript-7-native-server-support/spec|config/002]] — another config path where an opaque failure needed a typed explanation
- [[mcp/005-tool-capability-discoverability/spec|mcp/005]] — `get_tool_support` coverage semantics
- Code: `crates/mcpls-core/src/config/mod.rs` (`extract_extension_from_pattern`, `build_effective_extension_map`, `ServerConfig::validate`), `crates/mcpls-core/src/bridge/state.rs` (`detect_language`, `PLAINTEXT_LANGUAGE`), `crates/mcpls-core/src/error.rs` (`Error::NoServerForLanguage`)
- Docs: `docs/user-guide/configuration.md` (`file_patterns`), `docs/user-guide/getting-started.md`, `docs/user-guide/installation.md`
