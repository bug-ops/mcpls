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

This tool has no document to route on. mcpls sends it to the first server that explicitly claims `workspace_symbols` in `handles`, otherwise to the first catch-all server.

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
