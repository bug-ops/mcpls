# MCP Tools Reference

Complete reference for all 16 MCP tools provided by mcpls.

## Overview

mcpls exposes semantic code intelligence from Language Server Protocol (LSP) servers as MCP tools. Each tool corresponds to one or more LSP methods and provides rich code information to AI agents.

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

### Diagnostics & Formatting Tools

| Tool | LSP Method | Description |
|------|------------|-------------|
| [get_diagnostics](#get_diagnostics) | `textDocument/publishDiagnostics` | Compiler errors and warnings |
| [get_cached_diagnostics](#get_cached_diagnostics) | Cached notifications | Diagnostics from server push notifications |
| [format_document](#format_document) | `textDocument/formatting` | Document formatting |

### Refactoring Tools

| Tool | LSP Method | Description |
|------|------------|-------------|
| [rename_symbol](#rename_symbol) | `textDocument/rename` | Workspace-wide symbol renaming |
| [get_code_actions](#get_code_actions) | `textDocument/codeAction` | Quick fixes and refactorings |

### Call Hierarchy Tools

| Tool | LSP Method | Description |
|------|------------|-------------|
| [prepare_call_hierarchy](#prepare_call_hierarchy) | `textDocument/prepareCallHierarchy` | Prepare call hierarchy at position |
| [get_incoming_calls](#get_incoming_calls) | `callHierarchy/incomingCalls` | Functions that call the target |
| [get_outgoing_calls](#get_outgoing_calls) | `callHierarchy/outgoingCalls` | Functions called by the target |

### Server Monitoring Tools

| Tool | Description |
|------|-------------|
| [get_server_logs](#get_server_logs) | Get LSP server log messages |
| [get_server_messages](#get_server_messages) | Get LSP server show messages |

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
| `line` | integer | Yes | Line number (1-based) |
| `character` | integer | Yes | Character position (1-based, UTF-8) |

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
| `line` | integer | Yes | Line number (1-based) |
| `character` | integer | Yes | Character position (1-based, UTF-8) |

### Returns

Array of definition locations:

```json
[
  {
    "uri": "file:///absolute/path/to/definition.rs",
    "range": {
      "start": { "line": 5, "character": 0 },
      "end": { "line": 5, "character": 14 }
    }
  }
]
```

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
| `line` | integer | Yes | Line number (1-based) |
| `character` | integer | Yes | Character position (1-based, UTF-8) |
| `include_declaration` | boolean | No | Include the declaration site (default: false) |

### Returns

Array of reference locations:

```json
[
  {
    "uri": "file:///path/to/file1.rs",
    "range": {
      "start": { "line": 15, "character": 4 },
      "end": { "line": 15, "character": 8 }
    }
  },
  {
    "uri": "file:///path/to/file2.rs",
    "range": {
      "start": { "line": 42, "character": 10 },
      "end": { "line": 42, "character": 14 }
    }
  }
]
```

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

Get compiler errors, warnings, and hints for a file.

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

Array of diagnostic messages:

```json
[
  {
    "range": {
      "start": { "line": 10, "character": 8 },
      "end": { "line": 10, "character": 24 }
    },
    "severity": 1,
    "message": "cannot find value `undefined_variable` in this scope",
    "source": "rustc"
  },
  {
    "range": {
      "start": { "line": 15, "character": 0 },
      "end": { "line": 15, "character": 40 }
    },
    "severity": 2,
    "message": "unused variable: `x`",
    "source": "rustc"
  }
]
```

Severity levels:
- `1` - Error
- `2` - Warning
- `3` - Information
- `4` - Hint

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

- Diagnostics are updated automatically by the LSP server
- May include linter warnings (clippy for Rust, pylint for Python)
- Empty array if no issues found

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
| `line` | integer | Yes | Line number (1-based) |
| `character` | integer | Yes | Character position (1-based, UTF-8) |
| `new_name` | string | Yes | New name for the symbol |

### Returns

Workspace edit with all changes:

```json
{
  "changes": {
    "file:///path/to/file1.rs": [
      {
        "range": {
          "start": { "line": 10, "character": 4 },
          "end": { "line": 10, "character": 16 }
        },
        "newText": "new_identifier_name"
      }
    ],
    "file:///path/to/file2.rs": [
      {
        "range": {
          "start": { "line": 5, "character": 8 },
          "end": { "line": 5, "character": 20 }
        },
        "newText": "new_identifier_name"
      }
    ]
  }
}
```

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
| `tab_size` | integer | No | Tab size for formatting (default: 4) |
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
| `kind_filter` | string | No | Filter by kind (function, class, etc.) |
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

### Returns

Array of available code actions with edits.

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

Get diagnostics from LSP server push notifications (cached).

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
  "file_path": "/path/to/file.rs",
  "diagnostics": [
    {
      "message": "unused variable",
      "severity": "warning",
      "range": { "start": { "line": 10, "character": 5 }, "end": { "line": 10, "character": 10 } }
    }
  ]
}
```

### Notes

- Returns diagnostics pushed by LSP server via `textDocument/publishDiagnostics`
- More efficient than `get_diagnostics` as it uses cached data
- May be empty if file hasn't been analyzed yet

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
| `min_level` | string | No | Minimum level: error, warning, info, debug |

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

## Common Parameters

### file_path

**Type**: String
**Format**: Absolute path
**Validation**: Must exist within workspace roots

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
| Position out of bounds | Invalid line/character | Verify position is valid |
| Timeout | LSP server too slow | Increase timeout in config |
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
2. Increase timeouts for large codebases
3. Use file patterns to exclude build artifacts
4. Close unnecessary language servers

## Next Steps

- [Getting Started](getting-started.md) - Quick start guide
- [Configuration](configuration.md) - Configure language servers
- [Troubleshooting](troubleshooting.md) - Common issues and solutions
