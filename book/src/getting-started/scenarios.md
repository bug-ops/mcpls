# Everyday Scenarios

In this chapter you see how the tools combine into the tasks developers repeat every day. Each scenario shows a prompt you can paste, the tools the assistant uses, and why the result is more reliable than text search.

## Prerequisites

- A [working connection](connect-client.md) and [your first query](first-query.md).

## Understand unfamiliar code

> Give me an outline of `src/billing.rs` and explain what `calculate_total` does.

The assistant calls `get_document_symbols` for the structure of the file, then `get_hover` on `calculate_total` for its signature and documentation. Both come from the language server, so signatures are exact.

## Trace where something is used

> Where is `calculate_total` called, and which functions call it?

1. `get_references` lists every usage across the workspace.
2. `prepare_call_hierarchy` and `get_incoming_calls` show the calling functions, grouped by caller.

Add "and tell me which function each usage is inside" and the assistant passes `context: "enclosing_symbol"`, so every location comes with the function or type that contains it.

## Plan a safe rename

> Rename `process_data` to `handle_data` everywhere and show me the plan.

1. `prepare_rename` checks that the symbol can be renamed.
2. `rename_symbol` returns the edits for every file.

mcpls returns the edits; it does not write them. The assistant applies them with its own file tools, after you approve. If the result contains a `dropped` field, some edits were withheld (for example files outside the workspace) and the rename is incomplete, so the assistant should say so before applying anything.

## Fix compiler errors

> Fix the errors in `src/lib.rs`.

`get_diagnostics` provides the exact messages and ranges. `get_code_actions` can add the quick fixes the language server offers, such as adding a missing import.

## Navigate to the right implementation

> Which types implement the `Storage` trait?

`go_to_implementation` jumps from a trait method to its implementations; `prepare_type_hierarchy` with `get_subtypes` lists implementors for languages whose server supports type hierarchy. `go_to_type_definition` answers "what type is this value?" by jumping to the type itself.

## Look up a symbol you cannot place

> Where is something called `SessionManager` defined?

`workspace_symbol_search` searches names across the whole workspace, so the assistant does not need a file first.

## What's Next

Before you tune anything, read [Minimal Configuration](minimal-config.md) to see how little setup a first project needs.
