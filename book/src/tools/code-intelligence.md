# Code Intelligence

In this chapter you learn the tools for reading code: types, definitions, usages, outlines and completions. They answer "what is this, where does it come from, and who uses it". Addressing and result flags are explained in [Tools Overview](overview.md).

## Prerequisites

- You know how to [address code](overview.md#addressing-code).

## get_hover

Returns the type signature and documentation for the symbol at a position.

Arguments: `file_path`, and `line` with `character` or `symbol_name` (with optional `symbol_kind`, `container`).

```json
{ "file_path": "/work/app/src/models.rs", "line": 10, "character": 5 }
```

```json
{
  "contents": "```rust\nstruct User {\n    id: u64,\n    name: String,\n}\n```\n\nUser information structure.",
  "range": {
    "start": { "line": 10, "character": 5 },
    "end": { "line": 10, "character": 9 }
  }
}
```

The result is `null` when the server has no hover information for that position. Hover works best with statically typed languages.

## get_definition

Returns where a symbol is defined, across files and crates.

Arguments: the same addressing as `get_hover`, plus optional `context` (see [enclosing-symbol context](overview.md#enclosing-symbol-context)).

```json
{ "file_path": "/work/app/src/billing.rs", "symbol_name": "process_payment" }
```

```json
[
  {
    "uri": "file:///work/app/src/billing.rs",
    "range": {
      "start": { "line": 23, "character": 0 },
      "end": { "line": 23, "character": 14 }
    },
    "out_of_workspace": false
  }
]
```

A symbol with several definitions returns several locations; an unknown symbol returns an empty array.

## get_references

Finds every usage of a symbol in the workspace.

Arguments: addressing as above, plus `include_declaration` (default `false`) and optional `context`.

```json
{
  "file_path": "/work/app/src/errors.rs",
  "symbol_name": "Timeout",
  "container": "ApiError",
  "context": "enclosing_symbol"
}
```

Each location has `uri`, `range`, `out_of_workspace` and, with `context`, the `enclosing_symbol`. Searching a very common symbol can be slow, and the list is capped (`truncated: true`).

An empty result carries `index_coverage`: `synced_documents` is how many documents are currently synced to the server process (eviction of old documents lowers it), and `server_restarted` is `true` after a restart or respawn and stays `true` for the rest of the session. Some servers index only documents opened in the session, so the empty answer covers only what the server has indexed and is not proof that the symbol is unused. `get_definition` can still resolve such a symbol; touching the files of interest widens the answer.

## get_document_symbols

Returns the outline of one file as a hierarchy of symbols (types, functions, fields) with their ranges.

```json
{ "file_path": "/work/app/src/models.rs" }
```

Use it to understand a file before reading it, or to find the exact name for a `symbol_name` argument. Each symbol has a numeric `kind` such as `5` (class or struct), `6` (method), `8` (field), `11` (interface or trait) and `12` (function).

## workspace_symbol_search

Searches symbol names across the whole workspace, with partial and fuzzy matching. It needs no file, so it is the way in when you only know a name.

Arguments: `query` (required), `limit` (default 100, capped by the server) and `kind_filter` (a kind name such as `function` or `class`, or the numeric LSP kind).

```json
{ "query": "SessionManager", "kind_filter": "struct" }
```

This tool has no document to route on, so mcpls asks every server that may answer it: each server that claims `workspace_symbols` in `handles`, plus every catch-all server (one that omits `handles`), whatever its language. A server whose `handles` omit the tool is never asked, and neither is one that untrusted-workspace mode refused. With a single such server the result is exactly as described above.

With two or more servers the servers are queried concurrently, each within its own `request_timeout_seconds`, so one slow or dead server never delays or hides the others. The result then also carries:

- `server` on every symbol: the server that produced it.
- `servers`: one entry per asked server, in config order, with an `outcome` of `answered` (and `symbols`, the count it returned before `kind_filter` and `limit`), `initializing`, `unavailable` (failed to start, or its process died; the search never restarts it, so call `restart_server`), `timed_out`, `failed`, or `capability_not_advertised`. An `answered` entry with `symbols: 0` carries its own `index_coverage` note, and the top-level `index_coverage` is then omitted.

Symbols are concatenated in config order, a symbol that two servers report identically is kept once, and `kind_filter` and `limit` apply to the merged set (`truncated: true` marks a cut). A server that answers early cannot reorder the result. If no server answers, the call fails with the most specific error instead of returning an empty list, so an empty `symbols` next to a non-`answered` outcome means "that server could not say", not "no such symbol".

When the server itself answers with no symbols, the result carries the same `index_coverage` note as `get_references`. It is not added when `limit` or `kind_filter` emptied a non-empty server answer.

## get_document_highlights

Lists the occurrences of the symbol at a position inside the same file, each marked `read`, `write` or `text`. It is the file-local subset of `get_references` and is faster.

```json
{ "file_path": "/work/app/src/billing.rs", "line": 42, "character": 9 }
```

## get_signature_help

Returns the signatures and parameter documentation for the call at a position, with the active signature and parameter. Position it inside the parentheses of a call.

```json
{ "file_path": "/work/app/src/billing.rs", "line": 57, "character": 31 }
```

## get_completions

Returns completion candidates at a position. Add `trigger` (for example `.`, `:` or `->`) to mimic typing a trigger character.

```json
{ "file_path": "/work/app/src/billing.rs", "line": 57, "character": 18, "trigger": "." }
```

Each item has a `label`, a numeric `kind` (2 method, 3 function, 5 field, 6 variable, 7 class, 9 module), a `detail`, optional `documentation` and `insertText`. Completion requests are capped at 10 seconds whatever `request_timeout_seconds` says.

At most 100 items are returned, for every server. When a server returns more, mcpls ranks them by how well they match the identifier typed before the cursor (prefix matches first, then subsequence matches, each group ordered by the server's `sortText`, else by label) before cutting, and sets `truncated: true`; type more of the identifier to narrow the list. A server's own `isIncomplete` flag is not reflected in `truncated`.

## get_inlay_hints

Returns the inferred type and parameter-name annotations an editor would draw inline, for a range.

```json
{
  "file_path": "/work/app/src/billing.rs",
  "start_line": 40, "start_character": 1,
  "end_line": 60, "end_character": 1
}
```

Use it to see inferred types of `let` bindings without hovering over each one.

## What's Next

Continue with [Diagnostics and Formatting](diagnostics.md) to see what the compiler reports.
