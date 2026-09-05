# mcpls.toml — configuration reference

Compact field tables for `mcpls.toml`. This is a schema reference, not a tutorial —
for worked examples per language, see
[Configuration Reference](https://github.com/bug-ops/mcpls/blob/main/docs/user-guide/configuration.md)
and [Complete Examples](https://github.com/bug-ops/mcpls/blob/main/docs/user-guide/configuration.md#complete-examples).

## `[mcp]` fields

Overrides the text mcpls reports about itself over MCP; every field is optional and
independent, defaulting to mcpls's built-in text when omitted. `serverInfo.name`,
`version`, and `website_url` are not configurable here.

| Field | Type | Default | Max size | Notes |
|---|---|---|---|---|
| `title` | string | `"MCPLS - MCP to LSP Bridge"` | 128 bytes | Overrides `serverInfo.title`. |
| `description` | string | crate's `Cargo.toml` description | 1024 bytes | Overrides `serverInfo.description`. |
| `instructions` | string | built-in capability blurb | 4096 bytes | **Replaces** `ServerInfo.instructions` entirely — does not append to the built-in text. Read it at connection time instead of assuming the built-in blurb; see the note below. |

Limits are UTF-8 bytes, not characters, and apply to the raw configured string,
including surrounding whitespace — a whitespace-only value is rejected as empty
rather than checked against the byte cap. `tool_prefix` is not implemented yet.

## `[workspace]` fields

| Field | Type | Default | Notes |
|---|---|---|---|
| `roots` | array of strings | `[]` | Workspace root directories. Empty array auto-detects from the current directory. |
| `position_encodings` | array of strings | `["utf-8", "utf-16"]` | Preferred LSP position encodings (`utf-8`, `utf-16`, `utf-32`), offered to each spawned server during the `initialize` handshake in the listed order. A preference, not a restriction — per the LSP spec, UTF-16 is a mandatory fallback a server may still choose even if omitted here. |
| `language_extensions` | array of `{extensions, language_id}` | `[]` within an explicit `[workspace]` table; 30 built-in mappings only when `[workspace]` is absent entirely | Custom or overriding file-extension → language-ID mappings. Adding a `[workspace]` table for any other field (e.g. just `roots`) silently drops the 30 built-ins unless you list `language_extensions` yourself — list every language you need, not just the new one. |
| `heuristics_max_depth` | integer | `10` | Recursion depth for `heuristics.project_markers` search (see below). |

## `[[lsp_servers]]` fields

Each entry defines one language server; a language may have multiple entries (see
`handles` below).

| Field | Type | Required | Default | Notes |
|---|---|---|---|---|
| `language_id` | string | yes | — | e.g. `rust`, `python`, `typescript`. |
| `command` | string | yes | — | Executable name (resolved via `PATH`) or absolute path. |
| `args` | array of strings | no | `[]` | e.g. `["--stdio"]` for servers that need it. |
| `file_patterns` | array of glob strings | no | `[]` | e.g. `["**/*.rs"]`. Determines which files route to this server. |
| `name` | string | no | the `language_id` | Explicit routing identity. Required when two servers share one `language_id`, so each has a distinct identity. |
| `handles` | array of routing values | no | unset = catch-all | Restricts a server to specific tools; see [Tool routing](#tool-routing-handles) below. |
| `timeout_seconds` | integer | no | `30` | Timeout for the `initialize` handshake only. Rejects `0`. |
| `request_timeout_seconds` | integer | no | `30` | Timeout per individual LSP request after initialization (hover, definition, etc.), independent of `timeout_seconds`. Rejects `0`. Worst case per tool call: `4 * request_timeout_seconds + 3.5s` (retry budget on `-32802` responses). The completions timeout is `min(request_timeout_seconds, 10s)` — a value below 10 lowers the completions cap too; only values above 10 get clamped down to it. |
| `initialization_options` | table | no | `{}` | Server-specific options passed in the LSP `initialize` request, e.g. `cargo.features = "all"` for rust-analyzer. |
| `env` | table | no | `{}` | See [Environment passthrough](#environment-passthrough-env) below. |
| `heuristics.project_markers` | array of strings | no | unset | Marker files/directories that make this server applicable. mcpls searches for them recursively through the workspace tree up to `heuristics_max_depth` levels, skipping `node_modules`, `target`, and `.git`, e.g. `["pyproject.toml"]`. |

## Tool routing (`handles`)

`handles` values are routing identifiers, not MCP tool names — most match directly,
a few map many-to-one:

| `handles` value | MCP tool(s) it governs |
|---|---|
| `hover` | `get_hover` |
| `definition` | `get_definition` |
| `type_definition` | `go_to_type_definition` |
| `implementation` | `go_to_implementation` |
| `references` | `get_references` |
| `diagnostics` | `get_diagnostics` (pull) **and** `get_cached_diagnostics` (same route serves both) |
| `rename` | `rename_symbol` |
| `completions` | `get_completions` |
| `signature_help` | `get_signature_help` |
| `document_symbols` | `get_document_symbols` |
| `workspace_symbols` | `workspace_symbol_search` (see the special case below) |
| `format_document` | `format_document` |
| `code_actions` | `get_code_actions` |
| `call_hierarchy` | `prepare_call_hierarchy`, `get_incoming_calls`, `get_outgoing_calls`, sharing one route: an incoming/outgoing-calls lookup only makes sense against the server that produced the originating call-hierarchy item |
| `inlay_hints` | `get_inlay_hints` |

Rules:

- Each language may have one, and only one, server without `handles` set; that
  unrestricted server catches every tool the other servers for the language don't
  explicitly claim.
- A tool may be claimed by only one server per language.
- If the server routed to a tool fails to spawn, that tool falls back to the
  language's catch-all (if running); otherwise the call fails naming "no server
  available" rather than silently reaching a server that explicitly declined it via
  `handles`.
- `workspace_symbol_search` is the one tool with no document to route on. It
  resolves, across *all* configured servers, to the first explicit
  `workspace_symbols` claimant, else the first catch-all — there is no per-language
  fallback since the tool has no language. With neither, the call fails by name.

## Ambiguous configs fail at startup, not silently

A startup check looks for any pair of servers configured for the same language that
would both be active in the same workspace at once (per `heuristics.project_markers`).
If that pair also collides on routing — same `name`, both lacking `handles`, or both
claiming an identical tool — mcpls refuses to start rather than pick one arbitrarily,
and the error names the conflicting `[[lsp_servers]]` entries. Two servers whose
`heuristics.project_markers` are mutually exclusive never overlap in one workspace, so
that combination is not flagged and starts fine.

## Environment passthrough (`env`)

Spawned LSP server processes do **not** inherit mcpls's full environment — this is a
deliberate security boundary, not an oversight. Each child's environment is cleared,
then a minimal allowlist is passed through from mcpls's own process (`PATH`, `HOME`,
`USERPROFILE`, `TMPDIR`/`TEMP`/`TMP` on every platform, plus Windows loader
essentials), and only then is the server's `[lsp_servers.env]` table applied on top,
so entries there can override the passthrough.

Use `env` to restore anything a server needs beyond that allowlist: proxy settings,
`VIRTUAL_ENV`/`PYTHONPATH`, or toolchain variables a `build.rs` reads (`DATABASE_URL`,
`LIBCLANG_PATH`, `SSH_AUTH_SOCK`, …). Values here are written literally into
`mcpls.toml`, a file that's often committed to VCS — don't put real secrets in it;
and forwarding `SSH_AUTH_SOCK` hands the ssh-agent socket to the spawned LSP
process, so only do so for servers you trust.

**`PATH` caution:** an `env.PATH` entry overwrites the passthrough value rather than
extending it, and the two platforms then behave differently. On Unix, a bare
`command` with no directory component is now resolved against your override, so it
stops working unless you kept the original directory in it; on Windows the loader
still consults the parent process's `PATH` as a fallback after your override, so the
same mistake is less likely to break anything. If you just need to add one directory,
give `command` an absolute path instead of touching `PATH` at all.
