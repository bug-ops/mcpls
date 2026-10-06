# Diagnostics and Formatting

In this chapter you learn how to read the errors and warnings a language server reports, and how to ask for formatting and structural ranges. Diagnostics are what turn an assistant's "this should compile" into a checked fact.

## Prerequisites

- You know how to [address code](overview.md#addressing-code).

## get_diagnostics

Returns the errors, warnings and hints for one file, merged from a fresh pull request and the notifications the server has pushed (for example clippy results from rust-analyzer).

```json
{ "file_path": "/work/app/src/main.rs" }
```

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
    }
  ],
  "availability": "published",
  "origin": "pull",
  "indexing_in_progress": false,
  "push_notifications_degraded": false
}
```

Severity is `error`, `warning`, `information` or `hint`. Add `context: "enclosing_symbol"` to see which function each diagnostic is in.

Read the extra fields before you trust an empty list:

| Field | Values and meaning |
|-------|--------------------|
| `availability` | `published`: the server reported on the file, so an empty list means clean. `pending`: nothing was published yet. `evicted`: a publish was dropped to bound the cache. An empty list next to `pending` or `evicted` is not a clean file |
| `origin` | `pull` when a `textDocument/diagnostic` request answered, `push_cache` when the server has no pull support and the cache alone answered |
| `indexing_in_progress` | `true` if the server was still indexing during the read, so early errors may be false and real ones may be missing |
| `push_notifications_degraded` | `true` if the server crashed and restarted during this session, so push-only diagnostics may be missing until mcpls restarts |

Diagnostics from the two sources are deduplicated by severity, code and proximity (within 3 lines), without comparing messages. Two distinct errors with the same code that start within 3 lines appear once, with the pulled message.

## get_cached_diagnostics

Returns what mcpls already holds for a file, with no new analysis: the diagnostics the server pushed plus the last full report a `get_diagnostics` call stored. It is fast and takes only `file_path`.

While the file's server is still starting it returns the retryable `ServerInitializing` error, and if the server failed to start it returns that failure, instead of an empty list. An empty list therefore always means "no diagnostics". The same cache backs the `lsp-diagnostics://` resource; see [Diagnostics and Resources](../advanced/diagnostics.md).

## format_document

Returns the text edits that format a whole file.

Arguments: `file_path`, `tab_size` (1 to 32, default 4) and `insert_spaces` (default `true`).

```json
{ "file_path": "/work/app/src/main.rs", "tab_size": 4, "insert_spaces": true }
```

The edits are returned, not applied.

## format_range

Returns the edits that format only a range. Arguments: `file_path`, the four range fields, `tab_size` and `insert_spaces`. A line past the end of the file is rejected.

```json
{
  "file_path": "/work/app/src/main.rs",
  "start_line": 12, "start_character": 1,
  "end_line": 20, "end_character": 1
}
```

Not every server supports range formatting; rust-analyzer does not advertise it.

## get_selection_ranges

Returns the ranges that enclose a position, innermost first (expression, statement, block, function, and so on). Pass one of them to `get_code_actions` or `format_range`.

```json
{ "file_path": "/work/app/src/main.rs", "line": 14, "character": 9 }
```

## get_folding_ranges

Returns the foldable regions of a file with 1-based lines, a `kind` and collapsed text, sorted by start line with the longest first. Argument `kind` is `all` (default), `comment`, `imports` or `region`.

```json
{ "file_path": "/work/app/src/lib.rs", "kind": "imports" }
```

It is a compact way to see the shape of a large file.

## What's Next

Continue with [Refactoring](refactoring.md) to turn what you found into changes.
