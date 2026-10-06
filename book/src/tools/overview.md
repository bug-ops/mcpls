# Tools Overview

In this chapter you learn the conventions every mcpls tool shares: how code is addressed, what the advisory flags in results mean, and how to read errors. The next chapters then cover the 31 tools by task. Learn these rules once and every tool becomes predictable.

## Prerequisites

- A [connected client](../getting-started/connect-client.md).

## The 31 tools at a glance

| Task | Tools |
|------|-------|
| [Read and navigate code](code-intelligence.md) | `get_hover`, `get_definition`, `get_references`, `get_completions`, `get_document_symbols`, `workspace_symbol_search`, `get_document_highlights`, `get_signature_help`, `get_inlay_hints` |
| [Diagnostics and formatting](diagnostics.md) | `get_diagnostics`, `get_cached_diagnostics`, `format_document`, `format_range`, `get_folding_ranges`, `get_selection_ranges` |
| [Refactor](refactoring.md) | `prepare_rename`, `rename_symbol`, `get_code_actions` |
| [Hierarchies and navigation](hierarchies.md) | `go_to_implementation`, `go_to_type_definition`, `go_to_declaration`, `prepare_call_hierarchy`, `get_incoming_calls`, `get_outgoing_calls`, `prepare_type_hierarchy`, `get_supertypes`, `get_subtypes` |
| [Monitor and control servers](server-control.md) | `get_server_logs`, `get_server_messages`, `get_tool_support`, `restart_server` |

Every tool is read-only except `restart_server`, which stops and starts language server processes. Tools that produce edits (`rename_symbol`, `format_document`, `format_range`, `get_code_actions`) return them without applying them.

If you set `mcp.tool_prefix`, every name above gains the prefix, as in `{tool_prefix}_get_hover`.

## Addressing code

Most tools take a `file_path`, which must be an absolute path to a file under a workspace root.

Position-based tools use two integers:

- `line`: the line number, starting at 1.
- `character`: the column, starting at 1, counted in UTF-16 code units. For ASCII text this is the character count. See [Positions and Encodings](../advanced/positions.md) for the rest.

Range tools take `start_line`, `start_character`, `end_line` and `end_character` the same way. The end line must exist in the file.

### Addressing a symbol by name

`get_hover`, `get_definition`, `get_references`, `go_to_implementation`, `go_to_type_definition`, `prepare_call_hierarchy` and `rename_symbol` accept a symbol name instead of a position:

| Parameter | Meaning |
|-----------|---------|
| `symbol_name` | A symbol defined in the file; may be qualified, such as `Type::method` or `Type.method` |
| `symbol_kind` | Optional filter by kind (`function`, `method`, `struct`, ...) or numeric LSP `SymbolKind` |
| `container` | Optional filter: only symbols directly inside a type, impl, class or module of this name |

Give exactly one form. A request with both, neither, or half a position is rejected with `-32602`.

```json
{
  "file_path": "/work/app/src/parser.rs",
  "symbol_name": "Parser::parse",
  "symbol_kind": "method"
}
```

The result carries `resolved_symbol` with the `name`, `kind`, `container`, the position that was queried and `position_source` (`selection_range` or `inferred`). mcpls never guesses. A name that matches several symbols, none, or whose identifier cannot be located fails with `-32602` and a structured `data.resolution`:

| `resolution` | Meaning |
|--------------|---------|
| `ambiguous` | Several symbols match; every candidate is listed with its position |
| `not_found` | No symbol has that name |
| `not_defined_in_file` | The name is an import or a plain reference; use `workspace_symbol_search` or a position |
| `position_unverified` | The identifier could not be located unambiguously |

`rename_symbol` never produces edits for an ambiguous name. `get_signature_help` and `get_completions` take a position only.

## Reading results

### Truncation

List-returning tools are capped at a fixed maximum. When more exist, the result has `truncated: true`.

### Advisory flags

Two flags never block or filter a result; they tell the assistant when to be careful.

- **`out_of_workspace`** is set on each location that falls outside every workspace root, such as the standard library or a dependency. The location is valid; it is simply not under your roots.
- **`positions_degraded`** appears when a column could not be converted exactly for a server that does not use UTF-16. `"request"` means the position you sent reached the server unconverted, so the result may describe a different symbol and should not be trusted. `"response"` means only returned `character` offsets may be inexact. The flag is omitted when everything converted exactly.

### Indexing and diagnostics flags

Results that depend on the server's index carry `indexing_in_progress`, which is `true` when the server was still indexing during the call, so an empty or partial answer may be incomplete.

### Enclosing-symbol context

`get_references`, `get_definition`, `go_to_implementation`, `go_to_type_definition`, `go_to_declaration` and `get_diagnostics` accept `context`: `"none"` (the default, no extra work) or `"enclosing_symbol"`. With the second value, each location or diagnostic gains an `enclosing_symbol` naming the innermost symbol that contains it:

```json
{
  "uri": "file:///work/app/src/parser.rs",
  "range": { "start": { "line": 15, "character": 4 }, "end": { "line": 15, "character": 8 } },
  "enclosing_symbol": {
    "status": "resolved",
    "name_path": ["Parser", "parse"],
    "kind": 6,
    "range": { "start": { "line": 12, "character": 1 }, "end": { "line": 30, "character": 2 } },
    "fidelity": "hierarchical"
  }
}
```

| `status` | Meaning |
|----------|---------|
| `resolved` | The innermost containing symbol, with `name_path` (outermost first), LSP numeric `kind`, `range` and `fidelity` |
| `top_level` | The file's symbols were read and none contains the item |
| `not_computed` | Skipped, with a `reason`: `file_cap`, `out_of_workspace`, `tracker_limit` or `deadline` |
| `unavailable` | Attempted and failed, with a `reason`: `capability_absent`, `request_failed` or `timed_out` |

`not_computed` and `unavailable` mean nothing is known; never read them as top level. The lookup costs one `documentSymbol` request per distinct file, capped at 16 files (or `max_documents / 4` when `workspace.max_documents` is below 64) and a 30 second budget. An `enrichment` object reports `files_enriched`, `files_skipped` and `cut_short`. The primary result is never affected.

### Key naming

Keys that mcpls defines are `snake_case`. Objects passed through from the language server keep LSP's casing, such as the `Diagnostic` items and `selectionRange` on call hierarchy items.

### Arguments

A tool accepts only the arguments its schema declares. An argument name the tool does not declare, such as `kinds` instead of `kind_filter`, is rejected with an error that names the first unknown field and lists the accepted ones, instead of being ignored. Every `tools/list` input schema carries `additionalProperties: false`, so a schema-validating client sees the same rule. The `data` field of a call or type hierarchy item stays open, because it is opaque by contract.

Kind filters (`kind_filter` of `workspace_symbol_search` and `get_code_actions`, `symbol_kind` of the symbol-addressed tools, `kind` of `get_folding_ranges`) accept their names in any case, and an unknown or over-long value is rejected as `-32602` with the valid values.

## Errors

Failures are returned as MCP errors with a code and a message. Common ones:

| Situation | What to do |
|-----------|-----------|
| No server for the file's language | Add an `[[lsp_servers]]` entry, or install the server |
| `-32602` invalid params | Fix the path or parameters; a path outside every root is rejected |
| `failed to deserialize parameters: unknown field ...` | Use only the arguments the tool declares; the message lists them |
| Server still starting (`ServerInitializing`, `-32051`, retryable) | Wait and retry |
| Server still indexing (`-32050`, retryable) | Wait and retry, or raise `workspace.indexing_ready_timeout_seconds` |
| Server restarted while a request was in flight (`-32054`, retryable) | Retry the request |
| `capability_not_advertised` | The server lacks the feature; see `get_tool_support` |

See [Troubleshooting](../guide/troubleshooting.md) for the fixes.

## What's Next

Start with [Code Intelligence](code-intelligence.md), the tools you will use most.
