---
aliases:
  - Allowed host default-port rejection
tags:
  - sdd
  - spec
  - enhancement
  - http
  - security
created: 2026-10-05
status: implemented
related:
  - "[[constitution]]"
  - "[[mcp/012-http-typed-limits-origins-stream-deadline/spec|http-typed-limits-origins-stream-deadline]]"
---

# Feature: Reject default-port pins in allowed hosts

> [!info] Metadata
> **Type**: enhancement
> **Priority**: P3
> **Author**: Andrei G.
> **Issues**: #629
> **Observed at**: `517cb53`
> **Breaking**: yes (inputs accepted today become a parse error); document in `CHANGELOG.md`

## 1. Overview

### Problem Statement

`AllowedHost` ([[mcp/012-http-typed-limits-origins-stream-deadline/spec|mcp/012]] FR-011, FR-012)
accepts an optional pinned port. A pinned port must be present in the request `Host` and equal to
the pinned value; the transport's matcher does not infer a default port from a scheme, because a
`Host` value carries none.

Clients and proxies omit `:80` and `:443` from `Host`. A host stored as `x.example:443` or
`x.example:80` is therefore answered `403` for every client that omits the default port. The help
text, `CHANGELOG.md` and mcp/012 all say "never pin `:80` or `:443`", but nothing enforces it:

- `--http-allowed-host x.example:80`, `x.example:443`, `[::1]:443` and `1.2.3.4:80` start the
  server without error or warning, and the deployment fails only at request time with no startup
  diagnostic.
- `x.example:0` is also accepted, although port 0 is not a valid `Host` port and can never match.
- Every other malformed shape (wildcard, scheme, path, trailing dot, userinfo, non-ASCII, a port
  above 65535, an empty port, a missing host) is already rejected at argument parsing with a typed
  message, so this is the one documented-invalid shape that slips through.
- The same value reaches the server through `MCPLS_HTTP_ALLOWED_HOSTS` and through
  `HttpConfig::with_allowed_hosts` with a typed `AllowedHost`; `AllowedHost` has private fields and
  is built only by `FromStr`, so one parse-time rule covers all three entry points.

> [!note] Accuracy of "can never match"
> A pinned `:80` or `:443` matches a client that sends the port explicitly (for example a plain
> HTTP client addressed as `http://x.example:443/`, which keeps the port in `Host`). Such a client
> is rare and is not what an operator writing `x.example:443` normally intends; the dominant
> outcome is the silent `403`. The requirement below rejects the pin regardless.

### Goal

An `AllowedHost` can no longer carry port 80, 443 or 0. A value that would, fails at parse time
with a typed error that carries the guidance, on the CLI, the environment variable and the embedder
API alike, so the misconfiguration surfaces at startup instead of as request-time `403`s.

### Out of Scope

- Normalizing `:443` or `:80` to the portless form (decision in section 8; rejected).
- Allowing a deliberately pinned default port through an opt-in (for example a flag). This repeats
  the fail-open shape the project already rejects for unsafe defaults.
- Changing `AllowedOrigin`: an origin carries its scheme, so its default-port handling is lossless
  and unchanged (mcp/012 FR-004).
- Changing how the bound IP and the loopback names are admitted (mcp/012 FR-013); they are
  portless and unaffected.
- A TOML config surface for allowed hosts: none exists, so none is touched.
- Warning about a bind address on port 80 or 443: the bind port is unrelated to the pinned `Host`
  port.

## 2. User Stories

### US-001: Fail fast on a pin that cannot work

AS A operator configuring `--http-allowed-host` or `MCPLS_HTTP_ALLOWED_HOSTS`
I WANT a default-port pin rejected at startup with an explanation
SO THAT I do not discover the mistake as `403`s from every client after deployment.

**Acceptance criteria:**

```
GIVEN the HTTP transport is enabled
WHEN mcpls is started with --http-allowed-host x.example:443
THEN argument parsing fails with a usage error naming the problem and telling me to list the host without a port
AND no listener is opened
```

### US-002: Typed guarantee for embedders

AS A library embedder building an `HttpConfig`
I WANT `AllowedHost` to be unable to hold a default or zero port
SO THAT `with_allowed_hosts` cannot be handed a value that never matches.

**Acceptance criteria:**

```
GIVEN an embedder parses "x.example:80" into AllowedHost
WHEN the parse runs
THEN it returns Err(InvalidAllowedHost::DefaultPort) and no AllowedHost value exists to pass on
```

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-001 | WHEN an `AllowedHost` string parses to a pinned port of 80 or 443 THE SYSTEM SHALL return the typed error `InvalidAllowedHost::DefaultPort` | must |
| FR-002 | WHEN an `AllowedHost` string parses to a pinned port of 0 THE SYSTEM SHALL return a typed error (`InvalidAllowedHost::InvalidPort`, whose message changes to "a number from 1 to 65535") | must |
| FR-003 | THE `DefaultPort` message SHALL carry the guidance: ports 80 and 443 are omitted from `Host` by clients, so list the host without a port | must |
| FR-004 | THE port checks SHALL run on the parsed numeric value, after every structural check, so `x:080`, `x:0443`, `x:00` and `x:000080` are rejected like their canonical forms; a value that already fails an earlier check (wildcard, userinfo, non-ASCII, malformed, missing host, trailing dot, empty port, a port above 65535) keeps its current error | must |
| FR-005 | THE rule SHALL live in `FromStr for AllowedHost`, the only constructor that accepts a port, so `--http-allowed-host`, `MCPLS_HTTP_ALLOWED_HOSTS` (comma-separated, same value parser) and `HttpConfig::with_allowed_hosts` are covered without a second code path | must |
| FR-006 | THE stored port SHALL be unable to hold 0, 80 or 443: `AllowedHost` keeps its pinned port in a validated private newtype rather than a bare `u16`, so no in-crate constructor can build the illegal state | should |
| FR-007 | THE CLI SHALL surface the error as a usage error (exit code 2) through the existing `--http-allowed-host` value parser, in the same shape as the other `InvalidAllowedHost` variants, and a rejected value in `MCPLS_HTTP_ALLOWED_HOSTS` SHALL fail the whole invocation, not only that segment | must |
| FR-008 | IPv4 literals, bracketed IPv6 literals and host names SHALL all be subject to the rule: `1.2.3.4:80`, `[::1]:443`, `localhost:80` and `x.example:443` are rejected | must |
| FR-009 | A pinned port other than 0, 80 and 443 (for example `8080`, `8443`, `1`, `65535`) and any portless host SHALL keep parsing and displaying exactly as today | must |
| FR-010 | THE documentation SHALL be updated: `--http-allowed-host` help text, the `AllowedHost` and `InvalidAllowedHost` rustdoc, `crates/mcpls-cli/README.md`, `skills/mcpls/SKILL.md` and `docs/user-guide/configuration.md` (the list of values that are a usage error gains default ports and port 0; "never pin" becomes "rejected") | must |
| FR-011 | `CHANGELOG.md` SHALL record the change under `[Unreleased]` as a breaking change, in one line ending with the PR link | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Type safety | The illegal port states are unrepresentable in `AllowedHost`; the error is a typed enum variant, not a string |
| NFR-002 | Compatibility | `InvalidAllowedHost` is `#[non_exhaustive]`, so the new variant is not a source break for downstream matches; the behavior break (previously accepted strings now error) is documented in `CHANGELOG.md` |
| NFR-003 | Diagnosability | A misconfiguration is detected at startup, before any request is served |
| NFR-004 | Simplicity | No new dependency, flag or config key; the change is confined to `transport.rs` parsing, the CLI help text and docs |

## 5. Data Model

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| `AllowedHost` | A `Host` value allowed to reach the HTTP transport | lowercase host (IPv6 bracketed), optional pinned port in 1..=65535 excluding 80 and 443 |
| `InvalidAllowedHost` | Typed parse failure | gains `DefaultPort`; `InvalidPort` covers empty, above 65535 and 0 |

Resolved: `AllowedHost::port()` keeps returning `Option<u16>`; the value is validated by the private `PinnedPort` newtype.

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| `x.example:443`, `x.example:80` | `DefaultPort` with the guidance message |
| `[::1]:443`, `[2001:db8::1]:80` | `DefaultPort`; brackets do not change the port rule |
| `1.2.3.4:80`, `localhost:443` | `DefaultPort` |
| `X.EXAMPLE:443` | `DefaultPort`; case is irrelevant to the port check and the host is lowercased only on success |
| `  x.example:443  ` | Trimmed, then `DefaultPort` |
| `x.example:0443`, `x.example:080`, `x.example:000080` | `DefaultPort` (numeric value is checked) |
| `x.example:0`, `x.example:00` | `InvalidPort` |
| `x.example:65536`, `x.example:99999` | `InvalidPort` (unchanged; reported before the default-port check) |
| `x.example:` | `InvalidPort` (unchanged) |
| `:443` | `MissingHost` (unchanged; reported before the port check) |
| `*.example:443`, `u@x.example:443` | `Wildcard`, `UserInfo` (unchanged; first failing check wins) |
| `::1:443`, `[::1:443` | `Malformed` (unchanged; unbracketed IPv6 is not a host with a port) |
| `x.example:8443`, `x.example:81`, `x.example:442`, `x.example:1`, `x.example:65535` | Accepted and displayed unchanged |
| `x.example` | Accepted; matches any port (unchanged) |
| `MCPLS_HTTP_ALLOWED_HOSTS="a.example,b.example:443"` | Whole invocation fails on the second segment; the first segment is not applied |
| `MCPLS_HTTP_ALLOWED_HOSTS="a.example,,"` | Blank segments still ignored (unchanged) |
| Deployment that already stores `x.example:443` and upgrades | Fails at startup with the guidance instead of answering `403` to clients; the fix is to drop `:443` |
| A client that sends `Host: x.example:443` explicitly to a plain HTTP listener | Was matched by the pin before; after the change it matches only if `x.example` is listed without a port, which then admits any port on that host (see section 8) |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Unit test over the rejected set (`x.example:80`, `x.example:443`, `[::1]:443`, `1.2.3.4:80`, `x.example:0`, `x.example:0443`, mixed case, padded) | each returns the expected typed variant |
| SC-002 | Unit test over the accepted set (`x.example`, `x.example:8443`, `x.example:81`, `[::1]:8080`, `x.example:65535`) | parses and displays exactly as before |
| SC-003 | CLI parsing test for `--http-allowed-host` and `MCPLS_HTTP_ALLOWED_HOSTS` with a default-port value, alone and inside a comma list | usage error mentioning the guidance; valid segments alone still parse |
| SC-004 | Doc-test on `InvalidAllowedHost::DefaultPort` and the `AllowedHost` rustdoc example | passes under `cargo test --doc` |
| SC-005 | Existing mcp/012 end-to-end `Host` matrix (`spawn_http_server`) | unchanged results |
| SC-006 | Release binary, `--listen 127.0.0.1:<port> --http-allowed-host x.example:443` | exits with code 2 before binding, message contains the guidance |

## 8. Decision: reject or normalize an explicit `:443`

| Option | Behavior | Trade-off |
|--------|----------|-----------|
| A. Reject (recommended) | `x.example:443` is a parse error telling the operator to drop the port | Operator intent stays explicit; no silent widening; costs one startup error for deployments that wrote the pin by mistake, which is exactly the signal wanted |
| B. Normalize to portless | `x.example:443` is stored as `x.example` | Friendlier, no startup failure; but the stored allowlist is silently broader than what was written: a pin means "only this port", the portless form means "any port on this host", so one port becomes all ports |

Recommendation: A. The decisive asymmetry with `AllowedOrigin` (which does normalize the scheme
default port) is that an origin carries its scheme, so dropping `:443` from `https://host:443`
loses nothing, whereas a `Host` value has no scheme, so `:443` cannot be proven redundant and
dropping it changes what is allowed. Normalization also hides a documented mistake rather than
teaching the operator, and widens a DNS-rebinding allowlist without consent. Rejection with the
guidance message gives the same end state (the operator writes the portless form) with an explicit
decision.

Resolved: option A. A client that sends an explicit `:80` or `:443` in `Host` is accepted as a loss; list the host without a port.

## 9. Agent Boundaries

### Always (without asking)

- Run the four pre-commit checks from `CLAUDE.md` before committing
- Add the new variant to the `InvalidAllowedHost` rustdoc and doc-test
- Keep the error text free of references to external projects

### Ask First

- Changing the return type of the public `AllowedHost::port()` accessor
- Adding a flag or config key to permit a default-port pin
- Touching `AllowedOrigin` or the `Host` matching logic

### Never

- Normalize or silently drop a rejected port
- Introduce a second parsing path that can build an `AllowedHost` without the rule
- Edit `.claude/rules/continuous-improvement.md` for this behavior change; update `.local/testing/` playbooks, `coverage-status.md` and the regression list instead

## 10. Open Questions

None. mcp/012 now points here from its edge-case row.

## 11. See Also

- [[constitution]] -- project principles
- [[MOC-specs]] -- all specifications
- [[mcp/012-http-typed-limits-origins-stream-deadline/spec|mcp/012]] -- `AllowedHost` (FR-011, FR-012) and accepted `Host` values (FR-013)
- Code: `crates/mcpls-core/src/transport.rs` (`AllowedHost`, `InvalidAllowedHost`, `FromStr`), `crates/mcpls-cli/src/args.rs` (`--http-allowed-host`)
- Docs: `crates/mcpls-cli/README.md`, `skills/mcpls/SKILL.md`, `docs/user-guide/configuration.md`
