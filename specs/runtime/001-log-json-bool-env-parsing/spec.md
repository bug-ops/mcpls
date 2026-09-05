---
aliases:
  - log-json bool env parsing
  - MCPLS_LOG_JSON parsing bug
tags:
  - sdd
  - spec
  - cli
  - bug
created: 2026-08-05
status: implemented
related:
  - "[[constitution]]"
---

# Feature: Accept common boolean conventions for `--log-json`/`MCPLS_LOG_JSON` and `MCPLS_TRUST_PROJECT_CONFIG`

> [!info] Metadata
> **Author**: rust-ci-analyst (filed from live-testing evidence)
> **Branch**: fix/log-json-bool-env-parsing
> **Related PR**: #285 (commit c25d756) — introduced the regression by wiring `MCPLS_LOG_JSON` into `logging::init`
> **Related issue**: #279 (original request for `--log-json` support)

> [!success] Resolution
> Implemented by commit `5d9b871` (PR #314), closing #295. `crates/mcpls-cli/src/args.rs`
> now has a shared `parse_bool_flag` value parser applied to both `log_json` and
> `trust_project_config` via `#[arg(value_parser = parse_bool_flag)]`, matching FR-001–FR-008:
> accepts `true`/`false`, `1`/`0`, `yes`/`no` case-insensitively, rejects everything else with a
> named-accepted-values error, and keeps both defaults at `false`. Resolves all Open Questions in
> Section 9 in favor of the minimal-dependency custom `value_parser` approach (no new crate, no
> newtype). Test coverage added in `args.rs`'s existing `mod tests`
> (`test_parse_bool_flag_accepts_truthy_spellings`, `test_parse_bool_flag_accepts_falsy_spellings`,
> `test_parse_bool_flag_rejects_invalid_values`), per NFR-005.

## 1. Overview

### Problem Statement

`crates/mcpls-cli/src/args.rs` declares `log_json` and `trust_project_config` as plain `bool` fields with clap's `env` attribute:

```rust
#[arg(long, env = "MCPLS_TRUST_PROJECT_CONFIG")]
pub trust_project_config: bool,

#[arg(long, default_value = "false", env = "MCPLS_LOG_JSON")]
pub log_json: bool,
```

For a `bool`-typed field, clap derives its value parser from `str::parse::<bool>()`, which accepts **only** the exact lowercase literals `"true"` and `"false"`. Every other common boolean-env-var convention — `1`/`0`, `yes`/`no`, `TRUE`/`True`, `YES`, etc. — is rejected with a hard clap parse error, and the process exits immediately before starting. No LSP servers spawn, no MCP server starts, and nothing is logged in any format (not even JSON), because the failure happens during argument parsing, before `logging::init` ever runs.

This is a regression risk specifically because PR #285's entire purpose was to make `MCPLS_LOG_JSON` a real, production-facing observability control (closing #279). Operators wiring env vars via Docker, Kubernetes, or systemd unit files very commonly use `1`/`0` or `yes`/`no` conventions rather than the literal word `true`/`false`, and any case variation (`TRUE`, `True`) also fails today. The same narrow-parsing defect applies identically to `MCPLS_TRUST_PROJECT_CONFIG`, which has the identical `bool` + `env` declaration.

A secondary consequence: PR #285 also promised that "a crash stays JSON-parseable on stderr when `--log-json` is set." That guarantee does not hold for this failure mode — clap's own error is printed via its default, non-JSON, non-tracing error formatter, so a misconfigured `MCPLS_LOG_JSON` value produces exactly the kind of unstructured stderr output the flag exists to eliminate.

### Goal

Setting `MCPLS_LOG_JSON`/`--log-json` or `MCPLS_TRUST_PROJECT_CONFIG` to any of the common boolean-env-var conventions (`true`/`false`, `1`/`0`, `yes`/`no`, case-insensitive) is accepted and behaves predictably, without the process refusing to start.

### Out of Scope

- Changing the `MCPLS_LOG` / `--log-level` parsing (already permissive — validated later, not at parse time; see `test_log_level_case_sensitive`)
- Adding boolean-convention parsing to any other config surface (e.g. `mcpls.toml` fields) — this spec covers only the two CLI/env flags named above
- Redesigning `logging::init` or the crash-path JSON guarantee itself (only the arg-parsing entry point that gates it)
- Introducing a general-purpose "flexible bool" crate dependency without evaluating the custom-parser alternative first (see Open Questions)

## 2. User Stories

### US-001: Operator sets `MCPLS_LOG_JSON` via a `1`/`0` env convention
AS A platform operator deploying mcpls in Docker/Kubernetes/systemd
I WANT `MCPLS_LOG_JSON=1` (or `0`) to be accepted the same way `true`/`false` is
SO THAT I don't need to special-case mcpls's env var format against my organization's standard boolean-env-var convention

**Acceptance criteria:**
```
GIVEN the environment variable MCPLS_LOG_JSON=1
WHEN mcpls starts
THEN it starts successfully with JSON-formatted logging enabled (equivalent to MCPLS_LOG_JSON=true)

GIVEN the environment variable MCPLS_LOG_JSON=0
WHEN mcpls starts
THEN it starts successfully with compact (non-JSON) logging (equivalent to MCPLS_LOG_JSON=false)
```

### US-002: Operator sets `MCPLS_LOG_JSON` via a `yes`/`no` env convention
AS A platform operator
I WANT `MCPLS_LOG_JSON=yes` / `MCPLS_LOG_JSON=no` to be accepted
SO THAT common human-readable boolean conventions work without consulting mcpls-specific documentation

**Acceptance criteria:**
```
GIVEN the environment variable MCPLS_LOG_JSON=yes
WHEN mcpls starts
THEN it starts successfully with JSON-formatted logging enabled

GIVEN the environment variable MCPLS_LOG_JSON=no
WHEN mcpls starts
THEN it starts successfully with compact logging
```

### US-003: Operator sets a boolean env var with unexpected case
AS A platform operator whose env-file tooling uppercases values (e.g. `TRUE`, `True`)
I WANT case-insensitive matching for `true`/`false`/`1`/`0`/`yes`/`no`
SO THAT casing conventions from my env management tooling don't break startup

**Acceptance criteria:**
```
GIVEN the environment variable MCPLS_LOG_JSON=TRUE (or True, or YES, etc.)
WHEN mcpls starts
THEN it starts successfully with JSON-formatted logging enabled
```

### US-004: `MCPLS_TRUST_PROJECT_CONFIG` receives the same fix
AS A platform operator who also configures MCPLS_TRUST_PROJECT_CONFIG
I WANT the same accepted value set and case-insensitivity as MCPLS_LOG_JSON
SO THAT the two boolean-env flags behave consistently and I don't hit the same class of bug twice

**Acceptance criteria:**
```
GIVEN the environment variable MCPLS_TRUST_PROJECT_CONFIG=1
WHEN mcpls starts
THEN it starts successfully with project-local config trust enabled (equivalent to MCPLS_TRUST_PROJECT_CONFIG=true)
```

### US-005: Truly invalid value still fails clearly
AS A platform operator who mistypes a boolean env var (e.g. `MCPLS_LOG_JSON=maybe`)
I WANT a clear, actionable error message naming the accepted values
SO THAT I can fix the typo quickly instead of guessing why startup failed

**Acceptance criteria:**
```
GIVEN the environment variable MCPLS_LOG_JSON=maybe
WHEN mcpls starts
THEN the process exits nonzero with an error message that lists the accepted values (true/false, 1/0, yes/no, case-insensitive)
```

## 3. Functional Requirements

Use EARS notation. Prefix with FR-NNN.

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN `--log-json` or `MCPLS_LOG_JSON` is set to `true`, `1`, or `yes` (any case) THE SYSTEM SHALL enable JSON-formatted logging | must |
| FR-002 | WHEN `--log-json` or `MCPLS_LOG_JSON` is set to `false`, `0`, or `no` (any case) THE SYSTEM SHALL enable compact (non-JSON) logging | must |
| FR-003 | WHEN `--trust-project-config` or `MCPLS_TRUST_PROJECT_CONFIG` is set to `true`, `1`, or `yes` (any case) THE SYSTEM SHALL trust and load a project-local `mcpls.toml` | must |
| FR-004 | WHEN `--trust-project-config` or `MCPLS_TRUST_PROJECT_CONFIG` is set to `false`, `0`, or `no` (any case) THE SYSTEM SHALL NOT trust a project-local `mcpls.toml` | must |
| FR-005 | WHEN `MCPLS_LOG_JSON` or `MCPLS_TRUST_PROJECT_CONFIG` is set to a value outside the accepted set THE SYSTEM SHALL reject startup with an error message naming the accepted values | must |
| FR-006 | WHEN no value is supplied for `--log-json`/`MCPLS_LOG_JSON` THE SYSTEM SHALL default to JSON logging disabled (compact logging), preserving current default behavior | must |
| FR-007 | WHEN no value is supplied for `--trust-project-config`/`MCPLS_TRUST_PROJECT_CONFIG` THE SYSTEM SHALL default to trust disabled, preserving current default behavior | must |
| FR-008 | WHERE the fix changes argument parsing THE SYSTEM SHALL apply the identical accepted-value set and case-insensitivity to both `log_json` and `trust_project_config` (no divergence between the two flags) | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Compatibility | `MCPLS_LOG_JSON=true` and `MCPLS_LOG_JSON=false` (today's only accepted values) continue to work exactly as before — no regression for existing operators |
| NFR-002 | Compatibility | The bare `--log-json` / `--trust-project-config` flag (no `=value`, i.e. presence-implies-true CLI usage) continues to work exactly as before |
| NFR-003 | Usability | Error output for an invalid boolean value is human-readable and enumerates the accepted values; it does not require reading source code to resolve |
| NFR-004 | Maintainability | The parsing logic for both flags is not duplicated — implemented once and shared, or implemented via the same clap mechanism for both fields |
| NFR-005 | Testability | Unit tests cover every accepted value (`true`/`false`, `1`/`0`, `yes`/`no`) in at least two case variants (lowercase and one non-lowercase variant) for both flags, plus at least one rejected value |

## 5. Data Model

No new domain entities. This is a parsing-behavior change on two existing CLI/env-backed fields.

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| `Args::log_json` | CLI flag / env var controlling structured JSON logging | `bool`, sourced from `--log-json` or `MCPLS_LOG_JSON` |
| `Args::trust_project_config` | CLI flag / env var controlling whether a project-local `mcpls.toml` is trusted | `bool`, sourced from `--trust-project-config` or `MCPLS_TRUST_PROJECT_CONFIG` |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| `MCPLS_LOG_JSON=1` | Accepted, equivalent to `true` (FR-001) |
| `MCPLS_LOG_JSON=0` | Accepted, equivalent to `false` (FR-002) |
| `MCPLS_LOG_JSON=yes` / `no` | Accepted per FR-001/FR-002 |
| `MCPLS_LOG_JSON=TRUE` / `True` / `YES` | Accepted, case-insensitive (FR-001) |
| `MCPLS_LOG_JSON=maybe` / empty string / `2` | Rejected with a clear, non-cryptic error naming accepted values (FR-005); process still exits nonzero before startup, same as today's fail-fast behavior — only the error clarity changes, not the fail-fast contract |
| `--log-json` passed with no value (bare flag) | Continues to behave as `true` (unchanged, NFR-002) |
| Both `--log-json` CLI flag and `MCPLS_LOG_JSON` env var set to conflicting values | Unchanged from current clap precedence rules (CLI flag wins over env var) — this spec does not alter precedence, only value parsing |
| `MCPLS_TRUST_PROJECT_CONFIG` with any of the above values | Same behavior as `MCPLS_LOG_JSON`, mirrored per FR-003/FR-004/FR-008 |
| A future third boolean-env flag is added to `args.rs` | Should reuse the same shared parser (NFR-004) rather than reintroducing plain `bool` + clap default parsing |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | All reproduction cases from the filed issue (`TRUE`, `1`, `0`, `yes`, `no`) start mcpls successfully instead of erroring | 100% of listed cases pass |
| SC-002 | Existing accepted values (`true`, `false`, bare flag, default/unset) continue to behave identically to pre-fix behavior | No regression in existing `args.rs` test suite |
| SC-003 | New unit tests covering the accepted value matrix for both `log_json` and `trust_project_config` | All pass in `cargo nextest run --workspace --all-features` |
| SC-004 | Invalid value error message is inspectable in a test and contains the accepted-value list | Verified by at least one negative test case |

## 8. Agent Boundaries

### Always (without asking)
- Run `cargo +nightly fmt --all -- --check`, `cargo clippy --all-targets --all-features --workspace -- -D warnings`, and `cargo nextest run --workspace --all-features --lib --bins` before considering the fix complete
- Preserve existing default values (`log_json` defaults to `false`, `trust_project_config` defaults to `false`)
- Update `CHANGELOG.md` under `[Unreleased]`
- Update or extend the existing test module in `crates/mcpls-cli/src/args.rs` (`mod tests`) rather than creating a parallel test file, following existing patterns (e.g. `test_log_json_default_false`, `test_trust_project_config_flag`)
- Update the doc comments on `trust_project_config` (line 34-35) and `log_json` (line 45) to reflect the new accepted-value set, since the current doc comment explicitly (and now incorrectly) states "only the literal values `true`/`false` are accepted"

### Ask First
- Adding a new external crate dependency (e.g. a "flexible bool" parsing crate) if a custom `value_parser` function achieves the same result without one — prefer the zero-dependency approach per the project's Simplicity principle unless there's a strong reason
- Changing clap CLI-vs-env precedence behavior (out of scope, but touching the same attributes might tempt this)

### Never
- Change the default value of either flag (both must remain `false` by default)
- Silently swallow an invalid value instead of erroring (FR-005 requires a hard failure with a clear message — this is a parsing fix, not a "make everything permissive" change)
- Modify `logging::init` itself or the crash-path JSON stderr guarantee logic (out of scope; this spec is about the arg-parsing gate in front of it)

## 9. Open Questions

- [NEEDS CLARIFICATION: Should the fix use a custom clap `value_parser` function (e.g. `fn parse_bool_env(s: &str) -> Result<bool, String>`) applied via `#[arg(value_parser = parse_bool_env, ...)]`, keeping the field type as `bool`? This is the minimal-dependency approach consistent with the project's Simplicity principle and requires no new crate.]
- [NEEDS CLARIFICATION: Alternatively, should the project adopt a small typed wrapper (e.g. a local `EnvBool` newtype implementing `FromStr`) shared by both fields, to satisfy NFR-004 (no duplicated parsing logic) more explicitly than two separate `value_parser` attributes pointing at the same function? A single shared free function passed to both `#[arg(value_parser = ...)]` attributes likely satisfies NFR-004 without needing a newtype — recommend the free-function approach unless a third boolean-env flag appears in the near future.]
- [NEEDS CLARIFICATION: Exact wording of the rejected-value error message — should it match clap's existing error format style (`error: invalid value '<value>' for '--log-json'`) with an appended hint listing accepted values, or fully replace clap's default formatter? Recommend appending a `value_parser`-returned `Err(String)` describing accepted values, since clap renders custom `value_parser` error strings as part of its standard error output — this keeps error UX consistent with the rest of the CLI without requiring changes to error formatting infrastructure.]
- [NEEDS CLARIFICATION: Should `1`/`0`/`yes`/`no` also be documented in `--help` output (the `env = "MCPLS_LOG_JSON"` doc comment), or is updating the doc comment prose sufficient? Recommend updating the doc comment prose only, per the "Always" boundary above — clap's derive macro does not make it easy to inject a custom "possible values" list for a value_parser-backed bool field without additional attribute plumbing, and that plumbing is not required by any FR in this spec.]

## 10. See Also

- [[constitution]] — project principles (not yet created for this project)
- [[MOC-specs]] — all specifications
- PR #285 (commit c25d756) — introduced `MCPLS_LOG_JSON` wiring into `logging::init`, source of the regression
- Issue #279 — original request for `--log-json` support
- `crates/mcpls-cli/src/args.rs` — file containing both affected fields (`log_json` line 46-47, `trust_project_config` line 36-37)
