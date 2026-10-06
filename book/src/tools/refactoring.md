# Refactoring

In this chapter you learn the tools that propose changes: renames and code actions. They return edits and never write to disk, so the assistant stays in control of what changes and you stay in control of what is approved.

## Prerequisites

- You know how to [address code](overview.md#addressing-code).

## prepare_rename

Checks whether the symbol at a position can be renamed, before you propose a name.

```json
{ "file_path": "/work/app/src/billing.rs", "line": 23, "character": 8 }
```

The result `status` is one of:

| `status` | Meaning |
|----------|---------|
| `renameable` | The rename is valid; the result carries the identifier `range` and, when the server provides it, a `placeholder` |
| `default_behavior` | The server accepts the rename but reports no range |
| `not_renameable` | The position cannot be renamed; the server's reason may appear as `server_message` |

A server that does not support the check is refused with a capability error.

## rename_symbol

Returns the edits that rename a symbol everywhere it is used.

Arguments: `file_path`, `new_name`, and either `line` with `character` or `symbol_name` (with optional `symbol_kind` and `container`).

```json
{
  "file_path": "/work/app/src/data.rs",
  "symbol_name": "process_data",
  "new_name": "handle_data"
}
```

```json
{
  "changes": [
    {
      "uri": "file:///work/app/src/data.rs",
      "edits": [
        {
          "range": {
            "start": { "line": 10, "character": 4 },
            "end": { "line": 10, "character": 16 }
          },
          "new_text": "handle_data"
        }
      ]
    }
  ]
}
```

Check for `dropped` before applying anything. When some edits were withheld, because they point outside every workspace root, use an edit shape mcpls does not translate, or exceed the per-file cap (`exceeds_item_cap`), the result carries a per-reason count:

```json
{ "changes": [], "dropped": { "out_of_workspace": 1 } }
```

A non-empty `dropped` means the rename is incomplete even when `changes` is not empty. `dropped` is omitted when nothing was withheld.

An ambiguous `symbol_name` never produces edits; mcpls returns the candidates instead.

## get_code_actions

Returns the quick fixes, refactorings and source actions available for a range, with their edits.

Arguments: `file_path`, the four range fields, and optional `kind_filter` (`quickfix`, `refactor`, `source`, or a more specific kind, in any case).

```json
{
  "file_path": "/work/app/src/lib.rs",
  "start_line": 10, "start_character": 5,
  "end_line": 10, "end_character": 15,
  "kind_filter": "quickfix"
}
```

Use the range of a diagnostic from `get_diagnostics`, or a range from `get_selection_ranges`. The end line must exist in the file. An action's `edit.dropped`, when present and non-empty, means some of its changes were withheld, as for renames. The list is capped; `truncated: true` means some actions, diagnostics or edits were left out.

## What's Next

Continue with [Hierarchies and Navigation](hierarchies.md) to explore how code connects.
