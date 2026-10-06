# Hierarchies and Navigation

In this chapter you learn the tools that follow relationships: implementations, type definitions, declarations, call graphs and type hierarchies. They answer "what implements this", "who calls this" and "what does this extend".

## Prerequisites

- You know how to [address code](overview.md#addressing-code).

## Jump tools

These three take a position (and the first two also accept `symbol_name`) and return a list of locations with `uri`, `range` and `out_of_workspace`. All accept `context: "enclosing_symbol"`.

| Tool | Answers | Typical use |
|------|---------|-------------|
| `go_to_implementation` | Where is this trait method or interface member implemented? | Find implementors of a trait |
| `go_to_type_definition` | What type does this expression have, and where is it defined? | Jump from a variable to its type, not to its binding |
| `go_to_declaration` | Where is this symbol declared? | C and C++ headers, interface members; servers without the concept may return the definition |

```json
{ "file_path": "/work/app/src/store.rs", "symbol_name": "Storage::save" }
```

An empty result is valid. Lists are capped (`truncated: true`).

## Call hierarchy

Call hierarchy is a two-step conversation. First ask for an item, then walk from it.

1. `prepare_call_hierarchy` takes a position or `symbol_name` and returns callable items.
2. `get_incoming_calls` returns the callers of an item. `get_outgoing_calls` returns the functions it calls.

```json
{ "file_path": "/work/app/src/data.rs", "symbol_name": "initialize" }
```

Pass the `item` object to the next tool exactly as `prepare_call_hierarchy` returned it, unchanged: `{ "item": <the returned object> }`. An item is only meaningful to the server that produced it, so all three tools are routed together.

## Type hierarchy

The same pattern applies to inheritance and trait relationships:

1. `prepare_type_hierarchy` takes a position and returns type items.
2. `get_supertypes` returns the base classes and implemented interfaces of an item.
3. `get_subtypes` returns derived classes and implementors, one level at a time.

Feed an item back into either tool, including items they returned, to walk up or down the tree. Not every server supports type hierarchy: rust-analyzer and typescript-language-server do not advertise it, and the TypeScript 7 native server does not support it either. Ask `get_tool_support` first.

## What's Next

Finish the tool tour with [Server Monitoring and Control](server-control.md).
