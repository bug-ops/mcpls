---
aliases:
  - clangd launch defaults
  - clangd result limits
  - clangd silent result caps
tags:
  - sdd
  - spec
  - enhancement
  - config
  - clangd
  - truncation
created: 2026-11-01
status: draft
related:
  - "[[constitution]]"
  - "[[config/001-config-discovery-and-heuristics/spec|config/001-config-discovery-and-heuristics]]"
  - "[[config/003-unsupported-file-pattern-forms/spec|config/003-unsupported-file-pattern-forms]]"
  - "[[runtime/003-workspace-supplied-code-execution/spec|runtime/003-workspace-supplied-code-execution]]"
  - "[[mcp/014-tools-list-payload-size/spec|mcp/014-tools-list-payload-size]]"
  - "[[mcp/018-bounded-tool-inputs-and-results/spec|mcp/018-bounded-tool-inputs-and-results]]"
  - "[[mcp/008-manual-lsp-server-restart/spec|mcp/008-manual-lsp-server-restart]]"
  - "[[lsp/001-lsp-server-lifecycle-and-respawn/spec|lsp/001-lsp-server-lifecycle-and-respawn]]"
---

# Feature: clangd launch defaults that remove silent result cut-offs

> [!info] Metadata
> **Type**: enhancement
> **Priority**: P3
> **Author**: Andrei G.
> **Source**: continuous-improvement cycle, competitor gap analysis plus live reproduction
> **Issue**: #845

> [!abstract]
> The default clangd entry launches `clangd` with no arguments, so clangd's built-in result limits
> apply: 1000 references, 100 results for workspace symbols (and completion), 50 files for a rename.
> Those cut-offs happen inside the server, so mcpls does not know about them and its `truncated`
> flag stays `false`: a partial answer looks complete. This spec makes the default clangd launch
> lift the silent references and results caps to a bound that sits above mcpls's own documented
> caps (so only the flagged cap can bite), lowers clangd's log verbosity, and never duplicates or
> overrides an option the user already wrote in `args`.

## 1. Overview

### Problem Statement

`LspServerConfig::clangd()` (`crates/mcpls-core/src/config/server.rs`) builds the default entry
through the shared `builtin` constructor with an empty argument list. clangd then runs with its own
defaults, confirmed with `clangd --help`:

| clangd option | Default | Effect on mcpls tools |
|---------------|---------|-----------------------|
| `--limit-references` | 1000 | `get_references` returns at most 1000 locations; the server stops collecting |
| `--limit-results` | 100 | `workspace_symbol_search` returns at most 100 symbols; the same option also caps code completion |
| `--rename-file-limit` | 50 | `rename_symbol` over more than 50 files is refused by the server |
| `--log` | `info` | One stderr line per request and parse, including full compile commands |

**Reproduced.** Direct LSP client against clangd 21, scratch workspace, background index warm. A
header declares a function called 1200 times from a second translation unit.

| Request | Defaults | With the limit disabled (`0`) |
|---------|----------|-------------------------------|
| `textDocument/references` | 999 locations | 1202 locations |
| `workspace/symbol` for a prefix with about 1400 matches | 100 symbols | 1414 symbols |

**Why the cut-off is silent.** mcpls reports `truncated: true` only when its own caps cut a list:
`MAX_NORMALIZED_LOCATIONS` (10,000) for locations, goto results and the item count of workspace-edit
entries, and the `limit` argument of `workspace_symbol_search` (default 100, clamped to 10,000). A
server that stops at 999 locations or 100 symbols is indistinguishable from a complete answer. For
`workspace_symbol_search` the default tool `limit` equals clangd's default `--limit-results`, so
even the default call can never report `truncated: true` on clangd, however many symbols match.

**Log volume.** clangd writes its `--log` output to stderr. mcpls drains that stream for the whole
life of the process but keeps only the first 1 KiB and last 3 KiB (it feeds startup-failure and
exit excerpts). The `get_server_logs` ring (100 entries) is fed by `window/logMessage`, not by
stderr. So the claim that `--log=info` floods the `get_server_logs` buffer is unverified (section
9). What is certain: at `info` the retained stderr tail is made of request lines, which pushes
the actual failure text out of an excerpt, and the drain does needless work.

**Persisted defaults.** On first run mcpls serializes `Config::default()` to the user config file
(`create_default_config_file`). A change made only inside `LspServerConfig::clangd()` therefore
reaches new installs and regenerated files, and never reaches an existing file whose entry holds
`args = []`.

### Goal

A clangd server started from an entry that does not set these options answers references and
workspace-symbol requests without a server-side cut-off below mcpls's own cap, so any cut-off is
the one mcpls reports with `truncated: true`; its stderr is quiet by default; and an option the
user wrote is never repeated, overridden or reordered.

### Out of Scope

- Changing mcpls's own caps (`MAX_NORMALIZED_LOCATIONS`, the `limit` default of
  `workspace_symbol_search`) or the `truncated` semantics.
- Launch defaults for any server other than clangd. The mechanism may be reusable, but no other
  server gains an argument here.
- Adding a configuration key or environment variable to switch the defaults off. An explicit
  option in `args` is the only opt-out (section 9).
- Tuning clangd for performance (`--background-index`, `--pch-storage`, `-j`, `--clang-tidy`).
- Reporting a server-side cut-off the user chose by writing a lower limit in `args`.
- Technical design: recorded in a plan after this spec is approved.

## 2. User Stories

### US-001: A references answer is complete or says it is not

AS A user of an AI client on a C++ codebase
I WANT `get_references` on a widely used symbol to return every location, or `truncated: true`
SO THAT I never act on a silently partial list.

**Acceptance criteria:**
```
GIVEN the default clangd entry and a function called 1200 times
WHEN get_references is called on its declaration
THEN the result holds at least 1200 locations
  AND truncated is false
GIVEN the same server and a symbol with more locations than mcpls's cap
WHEN get_references is called
THEN the result holds exactly the cap and truncated is true
```

### US-002: Symbol search reports when the limit hid matches

AS A user searching a large C++ project for a name prefix
I WANT `truncated: true` when more symbols matched than were returned
SO THAT I narrow the query instead of assuming the 100 returned are all there are.

**Acceptance criteria:**
```
GIVEN the default clangd entry and a prefix with about 1400 matches
WHEN workspace_symbol_search is called with the default limit
THEN 100 symbols are returned and truncated is true
GIVEN the same query with limit 2000
THEN all 1414 symbols are returned and truncated is false
```

### US-003: My own clangd options are respected

AS A user who already tunes clangd through `args`
I WANT mcpls not to repeat or override what I wrote
SO THAT the server starts, and starts the way I configured it.

**Acceptance criteria:**
```
GIVEN args = ["--limit-references=50", "--log=verbose"]
WHEN the server is launched
THEN the final argument vector holds each of those two options exactly once, with my values
  AND no other launch-default option for them
GIVEN the spelling "--limit-references 50" or "-limit-references=50"
THEN the default for that option is still suppressed
```

### US-004: A quiet server and readable failure excerpts

AS A user diagnosing a clangd that fails to start
I WANT the stderr excerpt in the error to contain the failure, not request lines
SO THAT I can see why it failed.

**Acceptance criteria:**
```
GIVEN the default clangd entry
WHEN the server runs a session of 200 requests
THEN its stderr holds no per-request line at the default log level
```

### US-005: The documentation states the behavior

AS A user reading the guide
I WANT to see which options mcpls adds to clangd and how to change them
SO THAT I am not surprised by a result count or a log level.

**Acceptance criteria:**
```
GIVEN the language-servers and configuration pages of the user guide
WHEN they are read
THEN they list every option added to a default clangd launch, the rule that an option in args wins,
  and the spelling that overrides each one
```

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-001 | THE SYSTEM SHALL define the clangd launch defaults in one place as typed values (an option name and a typed value), not as strings assembled where the process is spawned | must |
| FR-002 | WHEN a server entry's command is clangd THE SYSTEM SHALL launch it with the launch defaults of FR-005 to FR-007 for every option that the entry's `args` does not already set | must |
| FR-003 | THE SYSTEM SHALL apply the same defaults on initial start, `restart_server` and automatic respawn, from the same function, so no launch path omits them | must |
| FR-004 | THE SYSTEM SHALL apply the defaults to an entry whose `args` come from a config file written before this change (an entry with `args = []` in a persisted default config), not only to newly generated entries | must |
| FR-005 | THE SYSTEM SHALL set the references limit so that the server-side bound exceeds mcpls's location cap (`MAX_NORMALIZED_LOCATIONS`) by at least one, or removes the bound; the exact value is open (section 9) | must |
| FR-006 | THE SYSTEM SHALL set the results limit so that the server-side bound exceeds the largest `limit` that `workspace_symbol_search` accepts after clamping by at least one, or removes the bound; the exact value is open (section 9) | must |
| FR-007 | THE SYSTEM SHALL lower clangd's log level from `info`; the level is open (section 9) | must |
| FR-008 | WHEN the user's `args` already name an option THE SYSTEM SHALL NOT add that option, whichever spelling the user used: `--name=value`, `-name=value`, `--name value`, `-name value`; the option name is compared whole, never by prefix | must |
| FR-009 | THE SYSTEM SHALL keep the user's `args` first, in their original order and unmodified, and SHALL append the defaults after them | must |
| FR-010 | WHEN the command is not clangd itself (a wrapper or launcher such as `nice`, `env`, `ccache` or a shell script whose target is clangd) THE SYSTEM SHALL NOT add any option, because the arguments belong to the wrapper | must |
| FR-011 | THE SYSTEM SHALL NOT write the launch defaults into the generated or persisted configuration, and SHALL NOT show them as part of the entry's `args` | should |
| FR-012 | WHEN untrusted-workspace mode is enabled THE SYSTEM SHALL vet the executable and the user's arguments exactly as before; the defaults are fixed literals chosen by mcpls, contain no path and cannot be supplied by workspace configuration, and SHALL be appended after the vetted arguments so that the indices recorded for wrapped programs stay valid | must |
| FR-013 | THE SYSTEM SHALL NOT change the `tools/list` payload: no tool name, schema or description changes with this feature | must |
| FR-014 | WHEN a server-side bound is above mcpls's cap THE existing `truncated` semantics SHALL hold unchanged: `truncated: true` means mcpls dropped items past its own cap or past the caller's `limit` | must |
| FR-015 | THE tool descriptions SHALL NOT change; the user guide SHALL state that on a default clangd a server-side cut-off no longer occurs below the cap, and that a limit the user writes in `args` is not reported as `truncated` | must |
| FR-016 | THE configuration and language-server pages of the user guide SHALL list the options added to a clangd launch, the precedence rule (FR-008, FR-009) and the spelling to override each, and SHALL replace the example whose `args` would now collide, if any | must |
| FR-017 | WHEN the installed clangd rejects a launch-default option THE SYSTEM SHALL NOT leave the language without a server silently; the behavior (probe, retry without defaults, or a documented minimum version) is open (section 9) | must |
| FR-018 | THE `.local/testing/` playbooks SHALL gain a case for the references count, the symbol count and `truncated`, the duplicate-spelling suppression, the quiet stderr, and the restart path; the coverage status for config and bridge SHALL be reset | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Type safety | A launch default is a typed value; detection of a user-written option parses `args` once into the same option-name type, with no `starts_with` string probing at the spawn site, per [[constitution]] |
| NFR-002 | Resource bound | With the defaults, the largest references or symbol answer a server can send stays well below the 10 MiB LSP frame limit in `lsp/transport.rs`; an answer over the frame limit must remain a diagnosable error and never a hang or truncation without notice |
| NFR-003 | Resource bound | Memory and time per request stay bounded by mcpls's caps and the existing request timeout; the feature adds no unbounded collection on a huge codebase |
| NFR-004 | Latency | A references request on the 1200-call-site fixture completes inside the default request timeout, and a default workspace-symbol query is not slower than 2x its time with clangd defaults on the same index |
| NFR-005 | Security | The defaults do not enlarge what an untrusted workspace can make mcpls execute; they are constant, contain no path or workspace-derived text, and are identical in trusted and untrusted mode |
| NFR-006 | Diagnosability | Startup logs at debug level the final argument vector's added options (names only), so a user can see what mcpls appended |
| NFR-007 | Compatibility | Pre-1.0 compatibility is not a constraint; the change in default result counts and stderr volume is recorded in `CHANGELOG.md` |
| NFR-008 | Cost | The check for user-written options is string inspection on already-loaded config; no spawn of `clangd --help` happens unless FR-017 chooses a probe, and a probe, if chosen, runs only after the executable has been vetted |

## 5. Data Model

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| `LspServerConfig` (existing) | A configured server entry | `command`, `args` (`ServerArgs`), `language_id` |
| Launch default (new, typed) | One option mcpls adds to a server it knows | option name (typed), value (typed: unlimited, bounded count, or log level) |
| Launch defaults set (new) | The ordered defaults of one server kind | keyed by server kind (clangd only here) |
| Final argument vector (derived) | User `args` followed by the defaults not already set | built at each spawn, never persisted (FR-011) |
| `MAX_NORMALIZED_LOCATIONS` (existing) | mcpls's cap of 10,000 items per list | the reference point for FR-005 and FR-006 |
| `WorkspaceSymbolResult.truncated`, `LocationsResult.truncated` (existing) | Cap flags | unchanged (FR-014) |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| `args = []` in an existing persisted config | Defaults still apply (FR-004) |
| `args = ["--limit-references=0"]` | Respected; no second `--limit-references`; the other defaults still apply |
| `args = ["--limit-references", "5"]` (separate value) | Detected as set; the `5` is not mistaken for a new option name |
| `args = ["--limit-references-extra=1"]` or `--log-file` style near-miss names | Not a match: names are compared whole (FR-008) |
| `args = ["--log=verbose"]` | Respected; the log default is not added |
| User sets a limit lower than mcpls's cap (`--limit-results=20`) | Respected; the result is cut by the server and `truncated` stays `false`; documented (FR-015) |
| `args` holds a response file (`@file`) | The content of the file is unknown; whether to skip all defaults for safety is open (section 9) |
| Command is `nice -n 5 clangd` or a wrapper script | No option added (FR-010) |
| Command is a versioned name (`clangd-18`) or `clangd.exe` | Whether it counts as clangd is open (section 9) |
| Two entries for clangd with different languages (`c`, `cpp`) | Each entry gets the defaults independently |
| `restart_server` or respawn after a crash | The same final vector as the first start (FR-003) |
| Untrusted mode, clangd allowed with `--allow-server` | Vetting runs on the user's args first; defaults appended afterward (FR-012); a refused server never starts and gains nothing |
| More references than mcpls's cap | `truncated: true` at the cap; the server bound is above the cap, so the cut is observable (FR-005) |
| Answer over the 10 MiB LSP frame limit | Surfaces as the existing frame-size error, naming the limit; not silently dropped (NFR-002) |
| Old clangd rejects `--limit-references` or repeats an option | See FR-017; the user is told, not left with no C++ language server |
| Completion on clangd | `--limit-results` is shared with completion: lifting it changes the completion list size too; see section 9 |
| `rename_symbol` over more than 50 files | Unchanged unless section 9 lifts `--rename-file-limit`; the server's refusal remains a loud error, not silent truncation |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | `get_references` on the 1200-call-site fixture, default clangd entry | At least 1200 locations (baseline 999), `truncated: false` |
| SC-002 | `workspace_symbol_search`, about 1400 matches, default `limit` | 100 symbols and `truncated: true` (baseline: `truncated: false`); with `limit` 2000, all matches, `truncated: false` |
| SC-003 | Symbol with more than 10,000 references | Exactly 10,000 locations and `truncated: true` |
| SC-004 | User `args` containing each of the four spellings of each option | The final argument vector holds that option exactly once, in the user's form; test covers every row of the spelling matrix |
| SC-005 | stderr bytes for 200 requests, default entry | At least 90% fewer bytes than at `--log=info`, and zero per-request lines |
| SC-006 | `tools/list` payload | Byte-identical to the golden `tool_surface.json`; the 136,000 B budget of [[mcp/014-tools-list-payload-size/spec\|mcp/014]] unchanged |
| SC-007 | Initial start, `restart_server` and respawn | The recorded spawn argument vectors are equal |
| SC-008 | Untrusted mode with and without the feature | The same admit or refuse decision for every row of the existing launcher and argument refusal tables |
| SC-009 | Documentation | Both guide pages list the added options and the override rule; the review finds no example that collides with a default |
| SC-010 | `CHANGELOG.md` | One-line entry with the PR link |

## 8. Agent Boundaries

### Always (without asking)
- Reproduce the 999 and 100 counts with the scratch fixture before changing behavior, and keep the fixture as a `.local/testing/` regression case.
- Keep the defaults in one typed definition and reuse it on start, restart and respawn.
- Run the full pre-commit suite, refresh `fuzz/Cargo.lock` if `mcpls-core` dependencies change, and update `CHANGELOG.md` with the PR link.
- Make new tests that create fake executables work on Windows (`.exe` names).

### Ask First
- Choosing `0` (unlimited) over a finite bound above the cap (section 9).
- Lifting `--rename-file-limit`.
- Running `clangd --help` or any probe process at startup (FR-017).
- Adding a configuration key or environment variable that disables the defaults (the untrusted-mode rule forbids an env var or key for that mode, and this one must not reintroduce a second control surface).
- Applying launch defaults to any server other than clangd.
- Changing mcpls's own caps or `truncated` semantics.

### Never
- Add an option the user already wrote, in any spelling, or reorder or modify the user's `args`.
- Add options to a wrapped or launcher command.
- Derive a default from workspace content or workspace config.
- Persist the defaults into the generated config file.
- Reference an external project or version in the spec text or the user docs beyond clangd itself.

## 9. Open Questions

> [!question] To resolve before `/sdd plan`

- [NEEDS CLARIFICATION: unlimited (`0`) or a finite bound above the cap? Recommended: a finite bound of cap + 1 for both options. It keeps the answer under the 10 MiB frame limit and bounds server memory and time on a huge codebase, while a cut at mcpls's cap is still visible as `truncated`. `0` is simpler but turns a very large answer into a frame-size error.]
- [NEEDS CLARIFICATION: `--limit-results` is shared with code completion. Does `get_completions` cap its own list? If not, lifting the option to a large value inflates every completion response; a lower bound that still covers `workspace_symbol_search` may conflict with a bound that suits completion. Decide whether to lift it for both, or accept a smaller workspace-symbol bound than the 10,000 cap.]
- [NEEDS CLARIFICATION: `--rename-file-limit` (default 50). The server's refusal is loud, not silent, so the finding is weaker than for references. Lift it to a finite bound (for example the cap), leave it and document the error plus the `args` override, or lift it only once a rename-specific error text exists? Recommended: leave unchanged in this spec and record a follow-up.]
- [NEEDS CLARIFICATION: exact log level. `error` gives the quietest stderr but may hide the warnings that explain a missing `compile_commands.json`; `info` is the current behavior; `verbose` is a user-chosen override. Recommended: `error`, confirmed by a live capture of what a startup failure prints at that level.]
- [NEEDS CLARIFICATION: does clangd emit `window/logMessage`? If it does, `get_server_logs` is fed by it and `--log` may or may not change that volume; if it does not, the buffer-flooding claim in the source finding is wrong and only the stderr tail excerpt benefits. Verify with a live capture of both streams and record the result in the playbook.]
- [NEEDS CLARIFICATION: minimum clangd version that accepts `--limit-references`, `--limit-results`, `--log` and tolerates the chosen spelling. An older clangd that rejects an unknown flag exits at startup, which would break the C++ language for those users. Options: documented minimum version; retry once without the defaults when the stderr excerpt reports an unknown argument; probe `--help` after vetting (costly, NFR-008). Pick one (FR-017).]
- [NEEDS CLARIFICATION: how is "the command is clangd" decided: the configured command's file name exactly `clangd`, also `clangd.exe`, also versioned names such as `clangd-18`, or the existing `BuiltinServer::Clangd` classification in `config/server.rs`? Reuse one existing classification rather than adding a second rule.]
- [NEEDS CLARIFICATION: response-file arguments (`@file`) in `args`: skip all defaults (safe against duplicates, loses the benefit) or ignore the file (may duplicate)? Recommended: skip the defaults and log it at debug level.]
- [NEEDS CLARIFICATION: real size of a 10,001-location references answer on the wire and its parse time, to confirm NFR-002 and NFR-004 with numbers instead of the estimate that it is far below 10 MiB.]

### Decisions taken in this draft

- Defaults are applied when the server is spawned, from the entry's command, rather than written into `clangd()`, because the generated config file is persisted on first run and an existing `args = []` would otherwise never receive them (FR-004).
- The defaults are appended after the user's arguments, not prepended, so indices into `args` that the untrusted-mode wrapper analysis records stay valid and user text keeps its position (FR-009, FR-012).
- An explicit option in `args` is the only opt-out; there is no new key or environment variable.

## 10. See Also

- [[constitution]] — project principles
- [[MOC-specs]] — all specifications
- [[config/001-config-discovery-and-heuristics/spec|config/001]] — built-in entries, persisted default config and `args` handling this builds on
- [[config/003-unsupported-file-pattern-forms/spec|config/003]] — an earlier fix for a silent config failure, same diagnosability goal
- [[runtime/003-workspace-supplied-code-execution/spec|runtime/003]] — untrusted-workspace executable and argument boundary (FR-012)
- [[mcp/014-tools-list-payload-size/spec|mcp/014]] — `tools/list` size budget (FR-013, SC-006)
- [[mcp/018-bounded-tool-inputs-and-results/spec|mcp/018]] — the caps and `truncated` semantics this relies on
- [[mcp/008-manual-lsp-server-restart/spec|mcp/008]] and [[lsp/001-lsp-server-lifecycle-and-respawn/spec|lsp/001]] — restart and respawn paths (FR-003)
- Code: `crates/mcpls-core/src/config/server.rs` (`LspServerConfig::clangd`, `builtin`), `crates/mcpls-core/src/config/server_args.rs` (`ServerArgs`), `crates/mcpls-core/src/config/mod.rs` (`create_default_config_file`), `crates/mcpls-core/src/lsp/vetting.rs` (`refuse_arguments`), `crates/mcpls-core/src/runtime/untrusted.rs`, `crates/mcpls-core/src/lsp/stderr.rs`, `crates/mcpls-core/src/bridge/translator/navigation.rs` (`MAX_NORMALIZED_LOCATIONS`), `crates/mcpls-core/src/bridge/translator/symbols.rs` (workspace symbol limit and `truncated`), `crates/mcpls-core/src/bridge/notifications.rs` (log ring)
- Docs: `book/src/guide/language-servers.md` (clangd example), `book/src/reference/config.md` (`args`), `book/src/tools/code-intelligence.md` (`truncated`)
