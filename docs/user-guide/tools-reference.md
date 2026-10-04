# MCP Tools Reference

Complete reference for all 29 MCP tools provided by mcpls.

## Overview

mcpls exposes semantic code intelligence from Language Server Protocol (LSP) servers as MCP tools. Each tool corresponds to one or more LSP methods and provides rich code information to AI agents.

Names below are the defaults; if the bridge is configured with `mcp.tool_prefix` (see
[Configuration Reference](configuration.md#mcp-section)), every tool name gains that prefix
(`{tool_prefix}_{tool}`).

### Addressing a Symbol by Name

`get_hover`, `get_definition`, `get_references`, `go_to_implementation`, `go_to_type_definition`,
`prepare_call_hierarchy` and `rename_symbol` accept either a position (`line` + `character`,
both 1-based) or a symbol name. Give exactly one form; a request with both, neither, or half a
position is rejected with `-32602`.

| Parameter | Type | Description |
|-----------|------|-------------|
| `symbol_name` | string | Name of a symbol defined in the file; may be qualified (`Type::method`, `Type.method`) |
| `symbol_kind` | string | Optional. Keep only symbols of this kind, by name (`function`, `method`, `struct`, ...) or numeric LSP `SymbolKind` |
| `container` | string | Optional. Keep only symbols directly inside a type, impl, class or module of this name |

The name is resolved against the file's `textDocument/documentSymbol` answer and the identifier
position is verified against the document text before the tool runs. A name-addressed result
carries `resolved_symbol` (`name`, `kind`, `container`, the queried `position`, and
`position_source`: `selection_range` or `inferred`). Resolution never guesses: a name that
matches several symbols, none, or whose identifier cannot be located unambiguously fails with
`-32602` and a structured `data.resolution` of `ambiguous` (with every candidate and its
position), `not_found`, `not_defined_in_file` (an import or plain reference; use
`workspace_symbol_search` or a position) or `position_unverified`. `rename_symbol` by name never
produces an edit for an ambiguous name. `get_signature_help` and `get_completions` take a
position only.

### Advisory Flags on Position-Bearing Results

Two flags can appear on results that carry file positions or locations, both purely advisory —
neither blocks or filters the result, they just tell the caller when to apply extra caution:

- **`out_of_workspace`** — set per-location on `get_definition`, `get_references`,
  `go_to_implementation`, `go_to_type_definition`, `get_incoming_calls`/`get_outgoing_calls`, and
  `workspace_symbol_search` results, `true` when that location falls outside every configured
  workspace root (e.g. it points into the standard library or a dependency). This is expected and
  common — the location is still valid, just outside the roots you configured.
- **`positions_degraded`** — set on any result where a column could not be converted exactly
  between MCP's UTF-16 columns and a non-UTF-16 server's encoding (disk-read budget exhausted, an
  unresolvable server-reported path, an oversized file, invalid UTF-8, a line past EOF, or a
  column inside a multi-unit character). It is a string and is omitted when every column converted
  exactly:
  - `"request"` — the queried position was sent to the server unconverted, so the result may
    describe a different symbol; do not trust it.
  - `"response"` — only returned `character` offsets may be inexact; the result still describes
    the symbol you asked about and can be kept.

  Column 1 and columns past the end of a line never trigger it (the latter clamp to the line end).
  For range tools (`get_inlay_hints`, `get_code_actions`), keep the range end inside the file.

### Enclosing-Symbol Context

`get_references`, `get_definition`, `go_to_implementation`, `go_to_type_definition` and
`get_diagnostics` accept an optional `context` parameter: `"none"` (the default) or
`"enclosing_symbol"`. With the default, the response is exactly what it was before the parameter
existed and mcpls issues no extra LSP request. `get_cached_diagnostics` does not accept it, because
it promises no new analysis. `go_to_declaration` does not accept it either (#608); other tools ignore an unknown `context` field. With `symbol_name` addressing the context applies to the returned locations as usual.

With `"enclosing_symbol"`, each location or diagnostic gains an `enclosing_symbol` field naming the
innermost symbol of its file that contains it, found with one `textDocument/documentSymbol` request
per distinct file (reused for every item in that file):

```json
{
  "uri": "file:///path/to/file.rs",
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
| `resolved` | The innermost containing symbol: `name_path` (outermost ancestor first), LSP numeric `kind` (the value `workspace_symbol_search` accepts as `kind_filter`), `range`, and `fidelity` |
| `top_level` | The file's symbols were read and none contains the item |
| `not_computed` | Skipped, with `reason`: `file_cap`, `out_of_workspace`, `tracker_limit` or `deadline` |
| `unavailable` | Attempted and failed, with `reason`: `capability_absent`, `request_failed` or `timed_out` |

`not_computed` and `unavailable` mean nothing is known: never read them as top level.
`fidelity: "flat"` means the server answered with the legacy flat symbol list, so the name path has
at most one container segment taken from `containerName`; `"hierarchical"` is a full ancestor chain.
A hit that straddles symbol boundaries is attributed to the innermost symbol containing the whole
range, else the innermost one containing its start.

The result also gains `enrichment`: `files_enriched`, `files_skipped` and `cut_short` (`true` when the
per-call file cap or the 30 second time budget skipped files). The cap is 16 distinct files, or
`max_documents / 4` (at least 1) when `workspace.max_documents` is below 64. Every file is validated
against the workspace roots before it is opened, so locations in the standard library or a
dependency come back `not_computed` with `out_of_workspace`; the primary result is never affected.
The `positions_degraded` flag also covers positions read during enrichment.

### Output Key Naming

Every key mcpls defines in tool results, in the `lsp-diagnostics://` resource payload, and in
retryable error `data` is `snake_case` (`indexing_in_progress`, `push_notifications_degraded`,
`server_id`, `elapsed_secs`), matching tool inputs. Objects passed through unchanged from the
language server keep LSP's own casing: the `Diagnostic` items inside the resource's `diagnostics`
array, `selectionRange` on call hierarchy items (so they round-trip into `get_incoming_calls` /
`get_outgoing_calls`), and the opaque `data` and command `arguments` fields.

## Tool Index

### Code Intelligence Tools

| Tool | LSP Method | Description |
|------|------------|-------------|
| [get_hover](#get_hover) | `textDocument/hover` | Type information and documentation |
| [get_definition](#get_definition) | `textDocument/definition` | Symbol definition location |
| [get_references](#get_references) | `textDocument/references` | All references to a symbol |
| [get_completions](#get_completions) | `textDocument/completion` | Code completion suggestions |
| [get_document_symbols](#get_document_symbols) | `textDocument/documentSymbol` | Document symbol outline |
| [workspace_symbol_search](#workspace_symbol_search) | `workspace/symbol` | Search symbols across workspace |
| [get_document_highlights](#get_document_highlights) | `textDocument/documentHighlight` | Read, write and text occurrences of a symbol in one file |

### Diagnostics & Formatting Tools

| Tool | LSP Method | Description |
|------|------------|-------------|
| [get_diagnostics](#get_diagnostics) | `textDocument/diagnostic` + push notifications | Compiler errors, warnings, and hints (merged from pull and push) |
| [get_cached_diagnostics](#get_cached_diagnostics) | Cached notifications | Diagnostics from server push notifications only |
| [format_document](#format_document) | `textDocument/formatting` | Document formatting |
| [format_range](#format_range) | `textDocument/rangeFormatting` | Formatting of a range |

### Refactoring Tools

| Tool | LSP Method | Description |
|------|------------|-------------|
| [prepare_rename](#prepare_rename) | `textDocument/prepareRename` | Check whether a position can be renamed |
| [rename_symbol](#rename_symbol) | `textDocument/rename` | Workspace-wide symbol renaming |
| [get_code_actions](#get_code_actions) | `textDocument/codeAction` | Quick fixes and refactorings |

### Call Hierarchy Tools

| Tool | LSP Method | Description |
|------|------------|-------------|
| [prepare_call_hierarchy](#prepare_call_hierarchy) | `textDocument/prepareCallHierarchy` | Prepare call hierarchy at position |
| [get_incoming_calls](#get_incoming_calls) | `callHierarchy/incomingCalls` | Functions that call the target |
| [get_outgoing_calls](#get_outgoing_calls) | `callHierarchy/outgoingCalls` | Functions called by the target |

### Type Hierarchy Tools

| Tool | LSP Method | Description |
|------|------------|-------------|
| [prepare_type_hierarchy](#prepare_type_hierarchy) | `textDocument/prepareTypeHierarchy` | Prepare type hierarchy at position |
| [get_supertypes](#get_supertypes) | `typeHierarchy/supertypes` | Supertypes of a type |
| [get_subtypes](#get_subtypes) | `typeHierarchy/subtypes` | Subtypes of a type |

### Navigation Tools

| Tool | LSP Method | Description |
|------|------------|-------------|
| [get_signature_help](#get_signature_help) | `textDocument/signatureHelp` | Parameter signatures at a call site |
| [go_to_implementation](#go_to_implementation) | `textDocument/implementation` | Jump to trait/interface implementations |
| [go_to_type_definition](#go_to_type_definition) | `textDocument/typeDefinition` | Jump to the type definition of a value |
| [go_to_declaration](#go_to_declaration) | `textDocument/declaration` | Jump to the declaration of a symbol |
| [get_inlay_hints](#get_inlay_hints) | `textDocument/inlayHint` | Inline type and parameter hints for a range |

### Server Monitoring & Control Tools

| Tool | Description |
|------|-------------|
| [restart_server](#restart_server) | Restart LSP servers (not read-only) |
| [get_server_logs](#get_server_logs) | Get LSP server log messages |
| [get_server_messages](#get_server_messages) | Get LSP server show messages |
| [get_tool_support](#get_tool_support) | Report which tools are usable for which languages |

---

## get_hover

Get type information and documentation for a symbol at a specific position.

### Parameters

```json
{
  "file_path": "/absolute/path/to/file.rs",
  "line": 10,
  "character": 5
}
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `file_path` | string | Yes | Absolute path to the file |
| `line` | integer | Yes, or `symbol_name` | Line number (1-based) |
| `character` | integer | Yes, or `symbol_name` | Character position (1-based, UTF-8) |

Instead of `line`/`character`, a `symbol_name` (with optional `symbol_kind` and `container`) may be given; see [Addressing a Symbol by Name](#addressing-a-symbol-by-name).

### Returns

JSON object with hover information:

```json
{
  "contents": "```rust\nstruct User {\n    id: u64,\n    name: String,\n}\n```\n\nUser information structure.",
  "range": {
    "start": { "line": 10, "character": 5 },
    "end": { "line": 10, "character": 9 }
  }
}
```

### Example Use Cases

**Claude interaction:**
```
User: What type is the variable user on line 42?
Claude: [Uses get_hover] The variable user has type User, a struct with fields
        id (u64), name (String), and email (String).
```

**Python type checking:**
```
User: What's the return type of calculate_total()?
Claude: [Uses get_hover] The function returns Optional[Decimal], which means
        it can return either a Decimal value or None.
```

### Notes

- Returns `null` if no hover information available
- Includes markdown-formatted documentation when available
- Works best with strongly-typed languages (Rust, TypeScript, Go)

---

## get_definition

Jump to the definition of a symbol at a specific position.

### Parameters

```json
{
  "file_path": "/absolute/path/to/file.rs",
  "line": 10,
  "character": 5
}
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `file_path` | string | Yes | Absolute path to the file |
| `line` | integer | Yes, or `symbol_name` | Line number (1-based) |
| `character` | integer | Yes, or `symbol_name` | Character position (1-based, UTF-8) |
| `context` | string | No | `none` (default) or `enclosing_symbol`; see [Enclosing-Symbol Context](#enclosing-symbol-context) |

Instead of `line`/`character`, a `symbol_name` (with optional `symbol_kind` and `container`) may be given; see [Addressing a Symbol by Name](#addressing-a-symbol-by-name).

### Returns

Array of definition locations:

```json
[
  {
    "uri": "file:///absolute/path/to/definition.rs",
    "range": {
      "start": { "line": 5, "character": 0 },
      "end": { "line": 5, "character": 14 }
    },
    "out_of_workspace": false
  }
]
```

See [Advisory Flags on Position-Bearing Results](#advisory-flags-on-position-bearing-results) for `out_of_workspace`.

### Example Use Cases

**Find function definition:**
```
User: Where is the process_payment function defined?
Claude: [Uses get_definition] The function is defined in src/billing.rs at line 23.
```

**Navigate to struct:**
```
User: Show me the User struct definition
Claude: [Uses get_definition] The User struct is defined in src/models/user.rs:
        [shows code snippet]
```

### Notes

- May return multiple locations for symbols with multiple definitions
- Returns empty array if no definition found
- Works across file boundaries

---

## get_references

Find all references to a symbol in the workspace.

### Parameters

```json
{
  "file_path": "/absolute/path/to/file.rs",
  "line": 10,
  "character": 5,
  "include_declaration": false
}
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `file_path` | string | Yes | Absolute path to the file |
| `line` | integer | Yes, or `symbol_name` | Line number (1-based) |
| `character` | integer | Yes, or `symbol_name` | Character position (1-based, UTF-8) |
| `include_declaration` | boolean | No | Include the declaration site (default: false) |
| `context` | string | No | `none` (default) or `enclosing_symbol`; see [Enclosing-Symbol Context](#enclosing-symbol-context) |

Instead of `line`/`character`, a `symbol_name` (with optional `symbol_kind` and `container`) may be given; see [Addressing a Symbol by Name](#addressing-a-symbol-by-name).

### Returns

Array of reference locations:

```json
[
  {
    "uri": "file:///path/to/file1.rs",
    "range": {
      "start": { "line": 15, "character": 4 },
      "end": { "line": 15, "character": 8 }
    },
    "out_of_workspace": false
  },
  {
    "uri": "file:///path/to/file2.rs",
    "range": {
      "start": { "line": 42, "character": 10 },
      "end": { "line": 42, "character": 14 }
    },
    "out_of_workspace": false
  }
]
```

See [Advisory Flags on Position-Bearing Results](#advisory-flags-on-position-bearing-results) for `out_of_workspace`.

### Example Use Cases

**Find all usages:**
```
User: Where is the calculate_total function used?
Claude: [Uses get_references] Found 7 references:
        1. src/billing.rs:45 - function call
        2. src/invoice.rs:23 - function call
        3. tests/billing_tests.rs:15 - test case
        [...]
```

**Impact analysis:**
```
User: If I change the User struct, what will be affected?
Claude: [Uses get_references] The User struct is referenced in 23 locations
        across 8 files, including models, services, and tests.
```

### Notes

- Searches entire workspace
- May be slow for frequently-used symbols
- `include_declaration: true` includes the definition site in results

---

## get_diagnostics

Get compiler errors, warnings, and hints for a file, including diagnostics from background analysis tools.

### Parameters

```json
{
  "file_path": "/absolute/path/to/file.rs"
}
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `file_path` | string | Yes | Absolute path to the file |
| `context` | string | No | `none` (default) or `enclosing_symbol`; see [Enclosing-Symbol Context](#enclosing-symbol-context) |

### Returns

An object with a `diagnostics` array plus `indexing_in_progress` and `push_notifications_degraded` flags:

```json
{
  "diagnostics": [
    {
      "range": {
        "start": { "line": 10, "character": 8 },
        "end": { "line": 10, "character": 24 }
      },
      "severity": "error",
      "message": "cannot find value `undefined_variable` in this scope",
      "code": "E0425"
    },
    {
      "range": {
        "start": { "line": 15, "character": 0 },
        "end": { "line": 15, "character": 40 }
      },
      "severity": "warning",
      "message": "unused variable: `x`",
      "code": "unused_variables"
    }
  ],
  "indexing_in_progress": false,
  "push_notifications_degraded": false
}
```

Severity levels: `error`, `warning`, `information`, `hint`.

`push_notifications_degraded` is `true` if the language server publishing this file's diagnostics crashed and was restarted during this mcpls session: diagnostics it delivers only by push (e.g. rust-analyzer's flycheck/clippy) are no longer received, so the result may be incomplete until mcpls restarts. Checked both before and after the underlying request, since the request itself can trigger a restart.

`indexing_in_progress` is `true` when the routed language server had an active signal showing its initial workspace indexing was still in progress at any point during this read (checked both before and after the underlying request, so a server that finishes mid-read is still caught) — the diagnostics above may reflect a partial index (still-loading references/types can surface as false errors, or a genuine error can be silently missing). `get_cached_diagnostics` and the `lsp-diagnostics://` resource carry the same flags. This is independent of the whole-workspace-query readiness gate other tools (`get_hover`, `get_definition`, etc.) block on, configured via `workspace.indexing_ready_timeout_seconds` (see [Configuration Reference](configuration.md)) — `get_diagnostics` never blocks on it, it only flags the result.

### Example Use Cases

**Check for errors:**
```
User: Are there any errors in this file?
Claude: [Uses get_diagnostics] Found 2 errors:
        Line 10: cannot find value `undefined_variable` in this scope
        Line 23: mismatched types: expected `i32`, found `String`
```

**Pre-commit validation:**
```
User: Is this code ready to commit?
Claude: [Uses get_diagnostics] Found 1 warning:
        Line 15: unused variable `x` - consider removing or prefixing with `_`
        Otherwise the code compiles successfully.
```

### Notes

- Returns diagnostics from both LSP pull requests and push notifications from background analysis tools
- Includes diagnostics from tools like clippy (Rust), pylint (Python), and other linters configured in your LSP server
- Diagnostics are automatically deduplicated by severity, code, and proximity to avoid duplicates across sources
- Empty array if no issues found
- If the LSP server is unavailable but diagnostics have been cached from previous push notifications, those cached diagnostics are returned

---

## rename_symbol

Rename a symbol across the entire workspace.

### Parameters

```json
{
  "file_path": "/absolute/path/to/file.rs",
  "line": 10,
  "character": 5,
  "new_name": "new_identifier_name"
}
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `file_path` | string | Yes | Absolute path to the file |
| `line` | integer | Yes, or `symbol_name` | Line number (1-based) |
| `character` | integer | Yes, or `symbol_name` | Character position (1-based, UTF-8) |
| `new_name` | string | Yes | New name for the symbol |

Instead of `line`/`character`, a `symbol_name` (with optional `symbol_kind` and `container`) may be given; see [Addressing a Symbol by Name](#addressing-a-symbol-by-name).

### Returns

Workspace edit with all changes:

```json
{
  "changes": [
    {
      "uri": "file:///path/to/file1.rs",
      "edits": [
        {
          "range": {
            "start": { "line": 10, "character": 4 },
            "end": { "line": 10, "character": 16 }
          },
          "new_text": "new_identifier_name"
        }
      ]
    },
    {
      "uri": "file:///path/to/file2.rs",
      "edits": [
        {
          "range": {
            "start": { "line": 5, "character": 8 },
            "end": { "line": 5, "character": 20 }
          },
          "new_text": "new_identifier_name"
        }
      ]
    }
  ]
}
```

If any edits from the language server had to be withheld (e.g. they referenced a file outside every configured workspace root, or used an edit shape mcpls does not translate), the result also carries a `dropped` field with a per-reason count:

```json
{
  "changes": [
    {
      "uri": "file:///path/to/file1.rs",
      "edits": [
        {
          "range": {
            "start": { "line": 10, "character": 4 },
            "end": { "line": 10, "character": 16 }
          },
          "new_text": "new_identifier_name"
        }
      ]
    }
  ],
  "dropped": { "out_of_workspace": 1 }
}
```

`dropped` is omitted entirely when nothing was withheld. A non-empty `dropped` means the rename is **incomplete** even when `changes` is non-empty -- do not apply `changes` as the full rename without checking for it first.

### Example Use Cases

**Rename function:**
```
User: Rename the process_data function to handle_data
Claude: [Uses rename_symbol] Prepared rename with 15 edits across 6 files:
        - src/data.rs: 3 edits
        - src/processor.rs: 8 edits
        - tests/data_tests.rs: 4 edits
        Would you like me to apply these changes?
```

**Refactor variable:**
```
User: Rename the user variable to customer throughout the codebase
Claude: [Uses rename_symbol] Found 47 occurrences across 12 files. This is
        a large refactoring. Shall I proceed?
```

### Notes

- Validates that the new name is a valid identifier
- Respects language-specific naming rules
- Does not apply changes automatically - returns edit plan
- Some LSP servers may reject invalid renames

---

## get_completions

Get code completion suggestions at a specific position.

### Parameters

```json
{
  "file_path": "/absolute/path/to/file.rs",
  "line": 10,
  "character": 5,
  "trigger": null
}
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `file_path` | string | Yes | Absolute path to the file |
| `line` | integer | Yes | Line number (1-based) |
| `character` | integer | Yes | Character position (1-based, UTF-8) |
| `trigger` | string | No | Trigger character (e.g., ".", ":", "->") |

### Returns

Array of completion items:

```json
[
  {
    "label": "to_string",
    "kind": 2,
    "detail": "fn(&self) -> String",
    "documentation": "Converts the value to a String.",
    "insertText": "to_string()"
  },
  {
    "label": "len",
    "kind": 2,
    "detail": "fn(&self) -> usize",
    "documentation": "Returns the length of the string.",
    "insertText": "len()"
  }
]
```

The result also carries `positions_degraded: "request"` (see [Advisory Flags](#advisory-flags-on-position-bearing-results)) when the queried position could not be converted exactly for a non-UTF-16 server.

Completion kinds:
- `1` - Text
- `2` - Method
- `3` - Function
- `5` - Field
- `6` - Variable
- `7` - Class
- `9` - Module

### Example Use Cases

**Method suggestions:**
```
User: What methods are available on this Vec?
Claude: [Uses get_completions] Available methods include:
        - push(value) - Add element to end
        - pop() - Remove and return last element
        - len() - Get number of elements
        - is_empty() - Check if empty
        [...]
```

**Import suggestions:**
```
User: How do I import HashMap?
Claude: [Uses get_completions] You can use:
        use std::collections::HashMap;
```

### Notes

- Completions are context-aware
- May be slow for large codebases
- Quality depends on LSP server capabilities

---

## get_document_symbols

Get an outline of all symbols in a document.

### Parameters

```json
{
  "file_path": "/absolute/path/to/file.rs"
}
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `file_path` | string | Yes | Absolute path to the file |

### Returns

Hierarchical array of symbols:

```json
[
  {
    "name": "User",
    "kind": 5,
    "range": {
      "start": { "line": 5, "character": 0 },
      "end": { "line": 10, "character": 1 }
    },
    "children": [
      {
        "name": "id",
        "kind": 8,
        "range": {
          "start": { "line": 6, "character": 4 },
          "end": { "line": 6, "character": 14 }
        }
      }
    ]
  },
  {
    "name": "create_user",
    "kind": 12,
    "range": {
      "start": { "line": 12, "character": 0 },
      "end": { "line": 20, "character": 1 }
    }
  }
]
```

Symbol kinds:
- `5` - Class/Struct
- `6` - Method
- `8` - Field
- `11` - Interface/Trait
- `12` - Function
- `13` - Variable

### Example Use Cases

**File overview:**
```
User: What's in this file?
Claude: [Uses get_document_symbols] The file contains:
        Structs:
        - User (lines 5-10) with fields: id, name, email
        - Config (lines 15-20)

        Functions:
        - create_user (line 25)
        - validate_email (line 40)
```

**Find specific symbol:**
```
User: What functions are exported from this module?
Claude: [Uses get_document_symbols] Public functions:
        - pub fn initialize() - line 10
        - pub fn process() - line 25
        - pub fn cleanup() - line 50
```

### Notes

- Returns hierarchical structure (children of classes, modules, etc.)
- Symbol visibility depends on LSP server
- Useful for navigation and code understanding

---

## format_document

Format a document according to language server rules.

### Parameters

```json
{
  "file_path": "/absolute/path/to/file.rs",
  "tab_size": 4,
  "insert_spaces": true
}
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `file_path` | string | Yes | Absolute path to the file |
| `tab_size` | integer | No | Tab size for formatting (default: 4); not bounded yet (#606) |
| `insert_spaces` | boolean | No | Use spaces instead of tabs (default: true) |

### Returns

Array of text edits to apply formatting:

```json
[
  {
    "range": {
      "start": { "line": 5, "character": 0 },
      "end": { "line": 5, "character": 45 }
    },
    "newText": "fn main() {\n    println!(\"Hello, world!\");\n}"
  }
]
```

### Example Use Cases

**Auto-format:**
```
User: Format this Rust file
Claude: [Uses format_document] Formatted according to rustfmt rules.
        Applied 12 formatting changes.
```

**Check formatting:**
```
User: Is this file properly formatted?
Claude: [Uses format_document] The file needs formatting changes:
        - Line 15: inconsistent indentation
        - Line 23: line too long (should wrap)
```

### Notes

- Uses language-specific formatter (rustfmt, black, prettier, etc.)
- Does not apply changes automatically - returns edit plan
- May fail if formatter is not available
- Respects `.editorconfig` and formatter configuration files

---

## workspace_symbol_search

Search for symbols across the entire workspace by name or pattern.

### Parameters

```json
{
  "query": "User",
  "kind_filter": null,
  "limit": 100
}
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `query` | string | Yes | Search query for symbol names |
| `kind_filter` | string | No | Filter by kind: a name (function, class, etc.) or the numeric `kind` value from a result |
| `limit` | integer | No | Maximum results (default: 100) |

### Returns

Array of matching symbols with locations.

### Example Use Cases

**Find type:**
```
User: Where is the Config struct defined?
Claude: [Uses workspace_symbol_search] Found Config in src/config.rs:15
```

---

## get_code_actions

Get available code actions (quick fixes, refactorings) for a range.

### Parameters

```json
{
  "file_path": "/path/to/file.rs",
  "start_line": 10,
  "start_character": 5,
  "end_line": 10,
  "end_character": 15,
  "kind_filter": null
}
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `file_path` | string | Yes | Absolute path to the file |
| `start_line` | integer | Yes | Start line (1-based) |
| `start_character` | integer | Yes | Start character (1-based) |
| `end_line` | integer | Yes | End line (1-based) |
| `end_character` | integer | Yes | End character (1-based) |
| `kind_filter` | string | No | Filter by action kind (quickfix, refactor, source) |

Keep the range end inside the file.

### Returns

Array of available code actions with edits. An action's `edit.dropped` field, when present and non-empty, means some of that action's changes were withheld (e.g. out-of-workspace files) -- see `rename_symbol`'s Returns section for the shape of `dropped`.

### Example Use Cases

**Quick fix:**
```
User: How can I fix this error?
Claude: [Uses get_code_actions] Available fixes:
        - Import missing module
        - Add derive macro
```

---

## prepare_call_hierarchy

Prepare call hierarchy at a position to get callable items.

### Parameters

```json
{
  "file_path": "/path/to/file.rs",
  "line": 10,
  "character": 5
}
```

### Returns

Array of call hierarchy items that can be used with `get_incoming_calls` or `get_outgoing_calls`.

---

## get_incoming_calls

Get functions that call the specified function (callers).

### Parameters

```json
{
  "item": { /* CallHierarchyItem from prepare_call_hierarchy */ }
}
```

### Example Use Cases

**Find callers:**
```
User: What functions call process_data?
Claude: [Uses get_incoming_calls] Found 5 callers:
        - main() in src/main.rs:10
        - run_batch() in src/batch.rs:25
```

---

## get_outgoing_calls

Get functions called by the specified function (callees).

### Parameters

```json
{
  "item": { /* CallHierarchyItem from prepare_call_hierarchy */ }
}
```

### Example Use Cases

**Analyze dependencies:**
```
User: What does initialize() call?
Claude: [Uses get_outgoing_calls] The function calls:
        - load_config()
        - connect_database()
        - start_server()
```

---

## get_cached_diagnostics

Get diagnostics from LSP server push notifications (cached), without making a new pull request.

### Parameters

```json
{
  "file_path": "/path/to/file.rs"
}
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `file_path` | string | Yes | Absolute path to the file |

### Returns

```json
{
  "diagnostics": [
    {
      "message": "unused variable",
      "severity": "warning",
      "range": { "start": { "line": 10, "character": 5 }, "end": { "line": 10, "character": 10 } }
    }
  ],
  "push_notifications_degraded": false,
  "indexing_in_progress": false
}
```

`push_notifications_degraded` is `true` if the file's routed server crashed and was respawned since it last published, so push-only diagnostics are no longer received and the cached diagnostics above may be incomplete until mcpls restarts. `indexing_in_progress` is the same signal `get_diagnostics` returns (see that tool's `Returns` section) -- `true` means the routed server was still indexing as of this read, so the cached diagnostics above may reflect a partial index.

### Notes

- Returns only diagnostics pushed by the LSP server via `textDocument/publishDiagnostics`, without making a new pull request
- Filtered by the same routing rules as `get_diagnostics`, so both tools use the same server when routed explicitly
- Returns an empty array if the file hasn't been analyzed yet or no push notifications have been received, but only once the file's server is running (or no server is configured for its language)
- If the server for the file's language failed to start, returns that startup error (`... failed to start: ...`, including the server's stderr when it printed any) instead of an empty array; the `lsp-diagnostics://` resource read behaves the same. Startup failures are not retried: fix the server and restart mcpls
- A server may publish diagnostics for one file under several spellings (a symlink and its target, for example). mcpls keys them by the file's canonical path and returns the union, with exact duplicates removed and entries ordered by range, so errors published under a symlink path are visible when you ask for the real path. A symlink pointing outside the workspace roots is ignored
- While the file's server is still starting, returns the retryable `ServerInitializing` error (code `-32051`) instead of an empty array; retry after a short wait
- The diagnostics resource read, `resources/subscribe` and `subscriptions/listen` follow the same rules: subscribing while the server is still starting succeeds, and a subscriber is sent one `resources/updated` when startup fails, after which a re-read returns the error
- Useful when you want fast, cached-only results without waiting for a fresh pull request

---

## get_server_logs

Get recent log messages from LSP servers.

### Parameters

```json
{
  "limit": 50,
  "min_level": "warning"
}
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `limit` | integer | No | Maximum entries to return (default: 50) |
| `min_level` | string | No | Minimum level, exactly one of the lowercase values `error`, `warning`, `info`, `debug`. Any other value, including a different casing such as `ERROR`, is rejected with an `isError` tool result naming the accepted values |

### Returns

```json
{
  "logs": [
    {
      "level": "warning",
      "message": "File not found in index",
      "timestamp": "2024-01-15T10:30:00Z"
    }
  ]
}
```

### Example Use Cases

**Debug LSP issues:**
```
User: Why isn't code completion working?
Claude: [Uses get_server_logs] Found error in LSP logs:
        "Failed to load project: Cargo.toml not found"
```

---

## get_server_messages

Get recent show messages from LSP servers.

### Parameters

```json
{
  "limit": 20
}
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `limit` | integer | No | Maximum entries to return (default: 20) |

### Returns

```json
{
  "messages": [
    {
      "type": "info",
      "message": "rust-analyzer is ready",
      "timestamp": "2024-01-15T10:30:00Z"
    }
  ]
}
```

### Notes

- Contains user-facing messages from LSP servers
- Useful for tracking server status and important notifications

---

## get_tool_support

Report which tools are usable for which languages in the current session, so an agent can avoid a call that is certain to be refused. Call it before using a tool on a new language.

### Parameters

```json
{
  "file_path": "/path/to/project/src/main.rs"
}
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `file_path` | string | No | Restrict the report to the language of this file (must exist and be inside a workspace root); pass it when the language is not obvious, e.g. `.tsx` files |

### Returns

```json
{
  "languages": ["python", "rust"],
  "tools": [
    {
      "name": "get_hover",
      "coverage": "some",
      "routes": [
        {"languages": ["python"], "status": "capability_not_advertised", "server": "pyright", "capability": "hoverProvider"},
        {"languages": ["rust"], "status": "supported", "server": "rust-analyzer"}
      ]
    },
    {"name": "get_diagnostics", "coverage": "all"},
    {"name": "get_server_logs", "coverage": "always"}
  ]
}
```

| Field | Values |
|-------|--------|
| `coverage` | `all` (every listed language), `some`, `none`, `unknown` (a server is still initializing), `always` (needs no language server) |
| `routes[].languages` | Languages sharing this route's outcome (absent for workspace-wide tools) |
| `routes[].status` | `supported`, `capability_not_advertised`, `initializing`, `no_server` |

### Notes

- `languages` lists every configured language (or only the language of `file_path`); an empty list means no server is configured
- `routes` is omitted for tools with `all` or `always` coverage; workspace-wide tools such as `workspace_symbol_search` have one route without `languages`
- Languages with an identical outcome (same `status`, `server` and `capability`) share one route entry, listed in `languages` in first-seen order, so the response stays compact with many configured languages
- `supported` means the call will be dispatched to a server that advertises the capability, not that it will succeed: indexing, push-only diagnostics, and respawn backoff can still fail it
- Capabilities a server registers dynamically after `initialize` are not reflected, here or in per-call enforcement
- `unknown` coverage is transient (a server is still starting): call the tool again later instead of caching the report
- Tool names include the configured `mcp.tool_prefix`

---

## get_signature_help

Get parameter signature information at a call site.

### Parameters

```json
{
  "file_path": "/path/to/file.rs",
  "line": 10,
  "character": 20
}
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `file_path` | string | Yes | Absolute path to the file |
| `line` | integer | Yes | Line number (1-based) |
| `character` | integer | Yes | Character position (1-based, UTF-8) |

### Returns

Signature help with active parameter highlighted:

```json
{
  "signatures": [
    {
      "label": "fn process(input: &str, timeout: u32) -> Result<Output>",
      "documentation": "Process the input string.",
      "parameters": [
        { "label": "input: &str" },
        { "label": "timeout: u32" }
      ],
      "activeParameter": 1
    }
  ],
  "activeSignature": 0,
  "activeParameter": 1
}
```

The result also carries `positions_degraded: "request"` (see [Advisory Flags](#advisory-flags-on-position-bearing-results)) when the queried position could not be converted exactly for a non-UTF-16 server.

### Notes

- Useful when the cursor is inside a function call's argument list
- Returns `null` if no signature information is available

---

## go_to_implementation

Jump to all implementations of a trait, interface, or abstract method.

### Parameters

```json
{
  "file_path": "/path/to/file.rs",
  "line": 10,
  "character": 5
}
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `file_path` | string | Yes | Absolute path to the file |
| `line` | integer | Yes, or `symbol_name` | Line number (1-based) |
| `character` | integer | Yes, or `symbol_name` | Character position (1-based, UTF-8) |
| `context` | string | No | `none` (default) or `enclosing_symbol`; see [Enclosing-Symbol Context](#enclosing-symbol-context) |

Instead of `line`/`character`, a `symbol_name` (with optional `symbol_kind` and `container`) may be given; see [Addressing a Symbol by Name](#addressing-a-symbol-by-name).

### Returns

Array of locations where the symbol is implemented:

```json
[
  {
    "uri": "file:///src/handlers/api_handler.rs",
    "range": {
      "start": { "line": 12, "character": 0 },
      "end": { "line": 12, "character": 28 }
    }
  }
]
```

### Example Use Cases

```
User: Show me all implementations of the Handler trait
Claude: [Uses go_to_implementation] Found 3 implementations:
        - src/handlers/api_handler.rs:12
        - src/handlers/db_handler.rs:8
        - src/handlers/file_handler.rs:5
```

---

## go_to_type_definition

Jump to the type definition of the value under the cursor (e.g. follow a typedef or type alias to its definition).

### Parameters

```json
{
  "file_path": "/path/to/file.rs",
  "line": 10,
  "character": 5
}
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `file_path` | string | Yes | Absolute path to the file |
| `line` | integer | Yes, or `symbol_name` | Line number (1-based) |
| `character` | integer | Yes, or `symbol_name` | Character position (1-based, UTF-8) |
| `context` | string | No | `none` (default) or `enclosing_symbol`; see [Enclosing-Symbol Context](#enclosing-symbol-context) |

Instead of `line`/`character`, a `symbol_name` (with optional `symbol_kind` and `container`) may be given; see [Addressing a Symbol by Name](#addressing-a-symbol-by-name).

### Returns

Array of type definition locations (same shape as [get_definition](#get_definition)).

### Notes

- Differs from `get_definition`: navigates to the *type* of an expression, not the expression itself
- Useful for following type aliases, `impl Trait` return types, or generic bounds

---

## go_to_declaration

Jump to the declaration of the symbol at a position (C/C++ headers, interface members). Servers
without a declaration concept may return the definition; an empty result is valid.

### Parameters

```json
{
  "file_path": "/path/to/file.cpp",
  "line": 10,
  "character": 5
}
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `file_path` | string | Yes | Absolute path to the file |
| `line` | integer | Yes | Line number (1-based) |
| `character` | integer | Yes | Character position (1-based) |

### Returns

Locations in the same shape as [go_to_implementation](#go_to_implementation), including
`truncated` and `positions_degraded`.

---

## get_inlay_hints

Get inline type and parameter hints for a range in a document.

### Parameters

```json
{
  "file_path": "/path/to/file.rs",
  "start_line": 1,
  "end_line": 50
}
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `file_path` | string | Yes | Absolute path to the file |
| `start_line` | integer | Yes | First line of range (1-based) |
| `end_line` | integer | Yes | Last line of range (1-based, inclusive) |

### Returns

Array of inlay hints with positions and labels:

```json
[
  {
    "position": { "line": 5, "character": 12 },
    "label": ": Vec<String>",
    "kind": 1
  },
  {
    "position": { "line": 8, "character": 24 },
    "label": "timeout:",
    "kind": 2
  }
]
```

### Notes

- Inlay hints show inferred types, parameter names, and other implicit information
- Request only the lines visible to the AI agent to keep response size manageable
- Keep the range end inside the file

---

## get_document_highlights

Find the occurrences of the symbol at a position within the same file, classified by how each uses the symbol.

### Parameters

```json
{
  "file_path": "/absolute/path/to/file.rs",
  "line": 10,
  "character": 5
}
```

### Returns

```json
{
  "highlights": [
    { "range": { "start": { "line": 10, "character": 5 }, "end": { "line": 10, "character": 9 } }, "kind": "write" },
    { "range": { "start": { "line": 14, "character": 12 }, "end": { "line": 14, "character": 16 } }, "kind": "read" }
  ]
}
```

`kind` is `read`, `write` or `text`; a server that omits the kind yields `text`. `truncated` and `positions_degraded` appear only when set.

### Notes

- Single-file analysis: valid while the server is still indexing
- Returns an empty list when the position names nothing
- Verified live on clangd and typescript-language-server

---

## format_range

Format only a range of a document.

### Parameters

```json
{
  "file_path": "/absolute/path/to/file.rs",
  "start_line": 5,
  "start_character": 1,
  "end_line": 12,
  "end_character": 1,
  "tab_size": 4,
  "insert_spaces": true
}
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `file_path` | string | Yes | Absolute path to the file |
| `start_line`, `start_character` | integer | Yes | Start of the range (1-based) |
| `end_line`, `end_character` | integer | Yes | End of the range (1-based) |
| `tab_size` | integer | No | Tab size for formatting (default: 4); not bounded yet (#606) |
| `insert_spaces` | boolean | No | Use spaces instead of tabs (default: true) |

### Returns

Same shape as `format_document`: an `edits` array of `{ range, new_text }` plus `positions_degraded` when set.

### Notes

- Returns an edit plan; nothing is applied
- Edits are not capped, like `format_document`
- The range is not checked against the document length: keep it inside the file (#607)
- Verified live on clangd and typescript-language-server; rust-analyzer does not advertise range formatting, so the call reports `capability_not_advertised` (see `get_tool_support`)

---

## prepare_rename

Check whether the symbol at a position can be renamed, before calling `rename_symbol`.

### Parameters

Same as `get_hover`: `file_path`, `line`, `character`.

### Returns

A `status` field selects the shape:

```json
{ "status": "renameable", "range": { "start": { "line": 3, "character": 8 }, "end": { "line": 3, "character": 15 } }, "placeholder": "counter" }
```

| `status` | Meaning |
|----------|---------|
| `renameable` | The identifier can be renamed; `range` is what a rename would replace and `placeholder` its current name when the server supplies it |
| `default_behavior` | The server accepts a rename here but leaves the identifier range to the client; no range is invented |
| `not_renameable` | The position cannot be renamed; `server_message` carries the server's explanation when it gave one |

### Notes

- Requires a server advertising `prepareProvider`; mcpls advertises `rename.prepareSupport` so servers such as typescript-language-server answer with a range
- Waits for indexing like `rename_symbol`, so a mid-index "not renameable" cannot mislead
- An out-of-range position stays an error and is not reported as `not_renameable`; servers that answer an out-of-range line with `null` read as `not_renameable` (#607)
- clangd reports "no symbol here" and "line out of range" as server error `-32001`, so on clangd a non-renameable position can surface as a server error rather than `not_renameable`

---

## prepare_type_hierarchy

Get the type hierarchy items at a position; feed an item to `get_supertypes` or `get_subtypes`.

### Parameters

Same as `get_hover`: `file_path`, `line`, `character`.

### Returns

```json
{
  "items": [
    {
      "name": "Derived",
      "kind": 5,
      "uri": "file:///path/to/main.cpp",
      "range": { "start": { "line": 8, "character": 1 }, "end": { "line": 8, "character": 30 } },
      "selectionRange": { "start": { "line": 8, "character": 8 }, "end": { "line": 8, "character": 15 } },
      "data": "opaque"
    }
  ]
}
```

`truncated`, `positions_degraded` and a per-item `out_of_workspace` appear only when set.

---

## get_supertypes

Get the supertypes of a type hierarchy item.

### Parameters

```json
{
  "item": { "name": "Derived", "kind": 5, "uri": "file:///path/to/main.cpp", "range": {}, "selectionRange": {} }
}
```

`item` is a typed object: pass back an item exactly as returned by `prepare_type_hierarchy`, `get_supertypes` or `get_subtypes`, including `data`.

### Returns

Same shape as `prepare_type_hierarchy`.

### Notes

- Waits for indexing; `prepare_type_hierarchy` does not
- Verified live on clangd. rust-analyzer and typescript-language-server do not advertise type hierarchy, so those calls report `capability_not_advertised`

---

## get_subtypes

Get the subtypes of a type hierarchy item. Parameters, returns and notes are the same as `get_supertypes`.

---

## Verifying an Edit

mcpls never applies edits itself and does not preview an edit before it is applied: a speculative-preview tool is a
deliberate non-goal, because an overlay on the document tracker would race other calls on the same
file and leak speculative diagnostics into the cache and resource subscriptions. To check an edit:

1. Get the edit from `rename_symbol`, `format_document`, `format_range` or `get_code_actions`, or write it directly.
2. Apply it with your own editor tooling and save the file.
3. Call `get_diagnostics`. Servers whose compiler diagnostics come from a build step (rust-analyzer's flycheck) report new errors only after the save, so an empty result right after the write can be stale: poll again, and check indexing with `get_tool_support`.
4. Revert with your own tools (for example `git checkout -- <file>`) if new errors appear.

The loop touches the working tree (file watchers, formatters on save, version-control status) and cannot run
where writes are denied.

---

## restart_server

Restart one or more LSP servers without restarting mcpls. The old process is stopped (graceful
`shutdown`/`exit`, then a kill of its whole process group on Unix or job object on Windows) and a
replacement is spawned and initialized from the server's existing configuration. Use it when a
server is wedged or serves a stale index, e.g. after editing `Cargo.toml` or `package.json`.

This is the only tool that is not read-only. It is annotated `readOnlyHint: false`, `destructiveHint: true` (the process-group kill also kills shared daemons other clients may use) and `idempotentHint: false`, so MCP clients should ask before running it.

### Parameters

```json
{ "servers": ["rust", "python"] }
```

or

```json
{ "all": true }
```

Give exactly one of `servers` (non-empty list of server ids) or `all: true`. An unknown id is
rejected with `-32602` listing the configured ids, and nothing is restarted.

### Returns

One entry per targeted server, sorted by id:

| `status` | Meaning |
|----------|---------|
| `restarted` | A fresh process is running. `indexing_state` is `unknown`, `loading` or `ready`; `coalesced: true` means another restart of the same server finished while this request waited; `push_notifications_degraded: true` means push diagnostics are still not live, so call it again |
| `failed` | The replacement did not start; `reason.kind` is `spawn_failed`, `initialize_failed` or `shutting_down`. The server stays registered and the next tool call retries under the crash-loop backoff |
| `throttled` | Restarted within the last 5 seconds; retry after `retry_in_ms` |
| `initializing` | The server has not finished its initial startup; retry shortly |
| `not_running` | The server never started (or failed at startup); fix the cause and restart mcpls |

### Notes

- Requests in flight on the old process fail with the retryable error code `-32054`
  (`server_restarted`); retry them.
- While the old process stops, the server's tools return the retryable `server_initializing` error.
- Documents are re-opened on the new process on next access, and the tool is gated on the new
  process's indexing readiness like any other.
- On Unix the whole process group is killed, including shared daemons the server started (for
  example the Gradle daemon behind jdtls); descendants that call `setsid()` may survive.

---

## Common Parameters

### file_path

**Type**: String
**Format**: Absolute path
**Validation**: Must be non-empty, free of NUL bytes, and exist within workspace roots

A path is accepted when it names a location under a workspace root through one of these spellings: the root's canonical (symlink-free) path, the root exactly as configured, or the logical working directory (`$PWD`) when it resolves to the same directory as the real working directory. The path is then resolved on disk and must still lie under a root, so a symlink inside a root that points elsewhere is rejected. Root-level system symlinks are admitted too: when a root lies under a link directly below `/` (for example `/tmp` on macOS, which points to `/private/tmp`), the link spelling of that root is accepted, after checking that it resolves to the same directory. Every other symlink spelling, including a link deeper in the tree such as `~/link`, is rejected with `PathOutsideWorkspace`, even though earlier versions accepted it; use one of the spellings above. The `out_of_workspace` result flag stays canonical-only (alias-aware flag: #605), so a location a server reports under a `/tmp` spelling can read `true` even though that spelling is admitted as input. Paths outside every root are rejected before the filesystem is consulted, so the error does not reveal whether such a file exists.

```json
{
  "file_path": "/Users/username/project/src/main.rs"  // Absolute
}
```

### line

**Type**: Integer
**Indexing**: 1-based (first line is 1)

```json
{
  "line": 10  // 10th line in the file
}
```

### character

**Type**: Integer
**Indexing**: 1-based (first character is 1)
**Encoding**: UTF-8 (converted to UTF-16 for LSP)

```json
{
  "character": 5  // 5th character (UTF-8 code points)
}
```

## Error Handling

All tools return errors in standard MCP error format:

```json
{
  "error": {
    "code": -32603,
    "message": "LSP server not available for file type 'rs'"
  }
}
```

Common error scenarios:

| Error | Cause | Solution |
|-------|-------|----------|
| LSP server not available | No server configured for file type | Add LSP server to config |
| File not found | File doesn't exist | Check file path |
| Invalid params (`-32602`) | `file_path` is empty, contains a NUL byte, or runs through a regular file (`main.rs/x`) | Fix the path |
| Position out of bounds | Invalid line/character | Verify position is valid |
| Timeout | LSP server too slow | Increase `request_timeout_seconds` in config |
| No hover information | Not hoverable | Try different position |

## Performance Considerations

### Slow Operations

- `get_references` - Searches entire workspace
- `rename_symbol` - Analyzes all files
- `get_completions` - May trigger indexing

### Fast Operations

- `get_hover` - Single file lookup
- `get_diagnostics` - Cached by LSP server
- `get_definition` - Direct index lookup

### Optimization Tips

1. Limit workspace roots to active projects
2. Increase `request_timeout_seconds` for large codebases
3. Use file patterns to exclude build artifacts
4. Close unnecessary language servers

## Next Steps

- [Getting Started](getting-started.md) - Quick start guide
- [Configuration](configuration.md) - Configure language servers
- [Troubleshooting](troubleshooting.md) - Common issues and solutions
