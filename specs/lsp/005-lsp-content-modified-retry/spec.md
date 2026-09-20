---
aliases:
  - LSP ContentModified Retry
  - ContentModified Retry
tags:
  - sdd
  - spec
  - bug
  - lsp-bridge
created: 2026-09-06
status: implemented
related:
  - "[[constitution]]"
---

# Feature: Retry LSP ContentModified (-32801) Errors Like ServerCancelled (-32802)

> [!info] Metadata
> **Author**: Andrei G.
> **Branch**: fix/lsp-content-modified-retry
> **Source**: live-testing cycle 025 finding (P2 bug)

> [!success] Resolution
> Implemented by commit `0a27791` (PR #390), closing #382. `LspClient::request` retries `-32801`
> (`ContentModified`) using the same attempt budget and backoff already applied to `-32802`
> (`ServerCancelled`), gated by a `CONTENT_MODIFIED_RETRY_METHODS` allowlist of read-only,
> idempotent requests — resolving FR-006's open question by scoping retry to safe methods rather
> than retrying unconditionally or reusing `should_retrigger`'s data-field gate. Mutating requests
> (rename, formatting, code actions) are excluded since retrying at a stale position could apply an
> edit the caller no longer expects. Also declares `general.staleRequestSupport.retryOnContentModified`
> during the initialize handshake from the same allowlist, and corrects the doc comment at
> `config/server.rs:185` that mislabeled `-32802` as "content modified" (FR-005).

## 1. Overview

### Problem Statement

`LspClient::request` (`crates/mcpls-core/src/lsp/client.rs`) already retries one
of the two transient "please retry, the document changed underneath this
request" error codes defined by the LSP specification: `ServerCancelled`
(`-32802`, `SERVER_CANCELLED_CODE` at client.rs:26). It retries up to
`SERVER_CANCELLED_MAX_RETRIES` (3) additional times, exponential backoff
starting at `SERVER_CANCELLED_INITIAL_DELAY_MS` (500 ms), gated by
`should_retrigger` (client.rs:429-441), which honors `data.retriggerRequest`
when present and defaults to retrying when absent.

The LSP spec defines a second, semantically identical code for the same
purpose: `ContentModified` (`-32801`). mcpls's retry logic does not cover it
at all. When real rust-analyzer returns `-32801`, the response is handled by
the generic error arm in `LspClient::request` (client.rs:401,
`Err(e) => return Err(e)`), which the bridge translator then wraps as a hard
`-32603` internal error and forwards straight to the MCP caller — no retry,
even though the very next identical request (issued moments later) succeeds.

Live testing against real rust-analyzer (v1.98.0) reproduced this
consistently: the first `get_references` call issued immediately after
`get_hover` polling confirmed the server had settled always failed with
`-32801 - content modified`, and every subsequent identical call (1s apart,
up to 7 attempts observed) succeeded. This is a systemic first-call-after-
settling pattern, not a rare flake.

Separately, `config/server.rs:185` mislabels `-32802` as "(content modified)"
in the `request_timeout_seconds` doc comment — per the LSP 3.17 spec, `-32802`
is `ServerCancelled` and `-32801` is the real `ContentModified`. Every other
reference to `-32802` in the codebase (client.rs:26, :230, :308, :389, :429,
:434; server.rs:238) correctly names it `ServerCancelled`, so this looks like
an isolated doc mislabel at server.rs:185, not a codebase-wide pattern. It may
still be the design-stage root cause of the gap: `-32802` and `-32801` appear
to have been conflated once, and only the (mislabeled) `-32802` path ended up
wired for retry.

> [!note] Verification against current `main` (commit 71d618e)
> Grepping the codebase confirms `config/server.rs:238` (the
> `MAX_TIMEOUT_SECONDS` doc comment, part of the same `RETRY_ATTEMPTS`-adjacent
> doc block cited in the original finding) already correctly labels `-32802`
> as `` `ServerCancelled` ``, not "content modified". Only `server.rs:185` is
> mislabeled today. This spec scopes the doc fix to that one line; see
> [[#9. Open Questions]].

### Goal

A `-32801` (`ContentModified`) error response from an LSP server is retried
automatically inside `LspClient::request`, transparent to the MCP caller,
using the same attempt count and backoff policy already applied to `-32802`
(`ServerCancelled`) — and the doc comment at `config/server.rs:185` correctly
names `-32802` as `ServerCancelled` instead of "content modified".

### Out of Scope

- Changing the retry policy's attempt count or backoff timing for the
  existing `-32802` path (no regression, but no tuning either).
- Adding retry coverage for any other LSP error code not defined by the spec
  as a "client should retry" signal.
- Changing how the bridge translator (`crates/mcpls-core/src/bridge/`) maps
  LSP errors to MCP errors — this feature only changes whether a retry
  happens before the translator ever sees a terminal error.
- Making the retry attempt count or backoff configurable via `LspServerConfig`
  (today it is a hardcoded constant for `-32802`; this feature keeps the same
  hardcoded-constant approach for `-32801`).

## 2. User Stories

### US-001: Transparent retry on transient content-modified errors

AS A coding agent calling mcpls tools (e.g. `get_references`, `get_hover`)
I WANT a transient `-32801` `ContentModified` response from the underlying LSP
server to be retried automatically
SO THAT I don't have to detect a spurious `-32603` failure and manually retry
a request that would have succeeded moments later.

**Acceptance criteria:**
```
GIVEN an LSP server returns a -32801 (ContentModified) error for a request
WHEN LspClient::request receives that error
THEN it retries the same logical request using the existing backoff policy
  (up to 3 additional attempts, starting at 500ms, doubling each attempt)
AND if a retry succeeds, the MCP caller receives the successful result with
  no visible error
AND if all retries are exhausted, the MCP caller receives the original
  -32801 error (translated to MCP format), not a different error
```

### US-002: Correct documentation of LSP error codes

AS A developer reading the mcpls codebase
I WANT the doc comment on `LspServerConfig::request_timeout_seconds`
(`config/server.rs:185`) to correctly name `-32802` as `ServerCancelled`
SO THAT future contributors don't conflate `-32801` and `-32802` again when
extending retry behavior or debugging LSP error handling.

**Acceptance criteria:**
```
GIVEN the doc comment at config/server.rs:185
WHEN it references error code -32802
THEN it names the code "ServerCancelled", not "content modified"
AND the doc's worst-case latency formula reflects that both -32801 and
  -32802 now share the same retry budget (see FR-002)
```

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN `LspClient::request` receives an LSP error response with code `-32801` (`ContentModified`) THE SYSTEM SHALL retry the request, following the same code path currently used for `-32802` (`ServerCancelled`) | must |
| FR-002 | WHEN retrying a `-32801` or `-32802` response THE SYSTEM SHALL apply one shared attempt budget: up to 3 additional attempts (4 total), backoff starting at 500ms and doubling per attempt — the two codes SHALL NOT each get their own independent 4-attempt budget within a single logical request | must |
| FR-003 | WHEN all retry attempts for a `-32801` response are exhausted THE SYSTEM SHALL return the original `-32801` LSP error (code, message, data preserved) to the caller, consistent with how exhausted `-32802` retries are surfaced today | must |
| FR-004 | WHEN a `-32802` (`ServerCancelled`) response includes `data.retriggerRequest == false` THE SYSTEM SHALL continue to return immediately without retry (existing behavior in `should_retrigger`, must not regress) | must |
| FR-005 | THE SYSTEM SHALL correct the doc comment at `config/server.rs:185` to name `-32802` as `` `ServerCancelled` `` rather than "content modified" | must |
| FR-006 | WHEN a `-32801` response is received THE SYSTEM SHALL retry it [NEEDS CLARIFICATION: should the retry be gated by a `data.retriggerRequest`-style flag as `-32802` is, or retried unconditionally? The LSP 3.17 spec does not define a `retriggerRequest` data field for `ContentModified`, only for `ServerCancelled`] | must |
| FR-007 | THE SYSTEM SHALL log a retry attempt for `-32801` at the same log level and with equivalent detail (method name, attempt number, backoff delay) as the existing `-32802` retry log at client.rs:338-341 and :388-391 | should |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Performance | Worst-case added latency for a single tool call must stay bounded: with a shared attempt budget (FR-002), the worst case remains `4 * request_timeout_seconds + 3.5s`, unchanged from today — it must not become `8 * request_timeout_seconds + 7s` by giving `-32801` and `-32802` independent budgets |
| NFR-002 | Reliability | The fix must not change behavior for any other LSP error code (e.g. `-32601` Method Not Found, `-32603` Internal Error) — only `-32801` gains new handling |
| NFR-003 | Observability | Retry attempts for `-32801` must be distinguishable from `-32802` retries in logs (distinct log message text), so operators can tell which transient condition is occurring in practice |
| NFR-004 | Testability | The fix must be verifiable with the same fake-LSP-server test harness already used for `-32802` retry tests (`client.rs`'s `retry_behavior` test module), without introducing a second, divergent test harness |

## 5. Data Model

No new persistent data entities. This feature only changes control flow and
error-code constants inside `LspClient::request`.

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| LSP JSON-RPC error response | Error payload returned by the LSP server for a request | `code` (i32), `message` (String), `data` (optional JSON value, e.g. `{"retriggerRequest": bool}`) |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Server returns `-32801` on attempt 1, then `-32802` on attempt 2, then succeeds on attempt 3 | Both codes count against the same shared attempt budget (FR-002); the request eventually succeeds and the caller sees no error |
| Server returns `-32801` on every attempt, including the last retry | Original `-32801` error (code, message, data) is returned to the caller unchanged, matching `-32802` exhaustion behavior (FR-003) |
| Server returns `-32801` with no `data` field at all | See FR-006 — behavior depends on resolution of that open question; LSP spec does not define `retriggerRequest` for `ContentModified` |
| Server returns `-32801` with `data.retriggerRequest == false` (non-spec-compliant server) | See FR-006 — if the same gating logic as `-32802` is reused, this would skip retry; needs explicit decision |
| Concurrent requests on the same client, some hitting `-32801` and some hitting `-32802` | Each request retries independently against its own attempt budget; no shared state between requests other than the same backoff constants |
| A `-32801` retry's backoff sleep races with the caller's own `timeout_duration` | Existing timeout-vs-retry interaction from `-32802` applies unchanged: `request()`'s outer `timeout()` wraps a single attempt, not the whole retry loop, so a slow attempt can still time out independent of retry logic |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | `get_references` (and other tools) no longer surface a bare `-32801`/`-32603` error on the first call after rust-analyzer settles, when a retry would have succeeded | 0 unretried `-32801` failures reaching the MCP caller in the live-testing reproduction scenario, across repeated runs |
| SC-002 | Existing `-32802` retry test suite (`client.rs`'s `retry_behavior` module) continues to pass unmodified in intent (assertions may need new constant names, but behavior must not regress) | 100% pass |
| SC-003 | New unit/integration test(s) cover `-32801` retry-success, retry-exhaustion, and (once FR-006 is resolved) the retrigger-gating behavior, using the existing fake-LSP-server harness | At least 2 new test cases added, mirroring the `-32802` coverage shape (`test_retry_exhaustion_returns_original_server_cancelled_error`, `test_retry_succeeds_after_one_server_cancelled_response`) |
| SC-004 | `cargo doc` / doc-link check confirms the corrected `config/server.rs:185` comment builds cleanly | `RUSTDOCFLAGS="--deny rustdoc::broken_intra_doc_links" cargo doc --no-deps` passes |

## 8. Agent Boundaries

### Always (without asking)
- Run the full check suite before considering the fix done: `cargo +nightly fmt --check`, `cargo clippy --all-targets --all-features --workspace -- -D warnings`, `cargo nextest run --workspace --all-features --lib --bins`, rustdoc gate
- Follow the existing pattern in `client.rs`'s `retry_behavior` test module (fake LSP server over `cat` subprocesses) for any new tests
- Update doc comments that reference the retry policy's attempt count or codes if the plan phase changes constant names

### Ask First
- Renaming `SERVER_CANCELLED_CODE`, `SERVER_CANCELLED_MAX_RETRIES`, or
  `SERVER_CANCELLED_INITIAL_DELAY_MS` to a more generic name (e.g.
  `TRANSIENT_RETRY_*`) — this is a reasonable refactor once a second code is
  covered, but changes public-ish internal naming and touches many call sites
- Any change to the attempt count or backoff timing values themselves
- Making the retry policy configurable via `LspServerConfig` (out of scope
  per [[#Out of Scope]], but flag if it seems warranted during implementation)

### Never
- Silently swallow a `-32801` error after exhausting retries — it must
  surface to the caller like `-32802` does today
- Change behavior for LSP error codes other than `-32801`
- Remove or weaken the existing `-32802` retry coverage while adding `-32801`

## 9. Open Questions

- [NEEDS CLARIFICATION: Does the LSP 3.17 spec's silence on a `retriggerRequest`-style data field for `ContentModified` (-32801) mean mcpls should retry unconditionally on `-32801`, or should it reuse `should_retrigger`'s gate (checking `data.retriggerRequest` if present, defaulting to retry if absent) for consistency with `-32802`? This determines FR-006's exact behavior.]
- [NEEDS CLARIFICATION: Should `-32801` and `-32802` share one internal constant/counter pair (e.g. rename to `TRANSIENT_RETRY_CODE`-style, matching on `code == SERVER_CANCELLED_CODE || code == CONTENT_MODIFIED_CODE`), or should each retain an independently named constant while sharing the same numeric budget? This is a code-organization decision better resolved during `/sdd plan`, but affects how FR-002's "shared attempt budget" is implemented.]
- [NEEDS CLARIFICATION: The original finding described a second mislabeled doc comment "near :234-240" in `config/server.rs`. Verification against current `main` (commit 71d618e) shows that block (the `MAX_TIMEOUT_SECONDS` doc comment) already correctly labels `-32802` as `ServerCancelled`. Confirm no second mislabel needs fixing, or point to the specific line if one still exists elsewhere.]
- [NEEDS CLARIFICATION: Should the worst-case latency doc comments (client.rs:230, :235-242; server.rs:186-189, :237-242) be updated in this same fix to mention `-32801` alongside `-32802`, or is that follow-up documentation work outside this bug's scope?]

## 10. See Also

- [[constitution]] — project principles
- [[MOC-specs]] — all specifications
- `crates/mcpls-core/src/lsp/client.rs` — `LspClient::request`, `SERVER_CANCELLED_CODE`, `should_retrigger`, `retry_behavior` test module
- `crates/mcpls-core/src/config/server.rs` — `LspServerConfig::request_timeout_seconds`, `MAX_TIMEOUT_SECONDS`
- [LSP 3.17 Specification](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/) — defines `ContentModified` (-32801) and `ServerCancelled` (-32802)
