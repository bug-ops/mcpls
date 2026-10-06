# Your First Query

In this chapter you ask the assistant three questions about your own code and see which mcpls tool answers each one. You will recognize the pattern: you ask in plain language, the assistant picks a tool, and mcpls returns facts from the language server.

## Prerequisites

- mcpls is [connected to your client](connect-client.md).
- A project that matches an installed language server. Rust needs no further setup; for other languages see [Minimal Configuration](minimal-config.md).

## Ask about a type

Open a project and ask:

> What is the return type of `process_request` in `src/handlers.rs`?

The assistant calls `get_hover` with the file and the symbol name, and answers from the language server:

```text
You:    What is the return type of process_request in src/handlers.rs?

Claude: [get_hover] It returns Result<Response, ApiError>.
        Response is defined in src/types.rs:23.
```

The first query after startup can take a few seconds while the language server indexes the project. mcpls waits for indexing to finish (up to 30 seconds by default) rather than answering from a half-built index.

## Find every usage

> Find everywhere `ApiError::Timeout` is handled.

```text
Claude: [get_references] Found 4 matches:
        - src/handlers/api.rs:89
        - src/handlers/api.rs:156
        - src/middleware/timeout.rs:34
        - tests/api_tests.rs:201
```

This is a semantic search: it finds real uses of that variant and skips comments, strings and unrelated names.

## Check for errors

> Are there compiler errors in `src/main.rs`?

```text
Claude: [get_diagnostics] 2 errors:
        - line 23: cannot find value `undefined_variable` in this scope
        - line 45: mismatched types: expected `i32`, found `String`
```

These are the messages the compiler reports, with the line they refer to.

## How the assistant addresses code

Tools that point at code take a `file_path` (an absolute path) and either a position or a symbol name:

- `line` and `character`, both starting at 1.
- `symbol_name`, such as `process_request` or `Parser::parse`, for a symbol defined in that file.

You rarely write these yourself. If a name matches more than one symbol, mcpls reports the candidates instead of guessing, and the assistant retries with a more specific request. Details are in [Tools Overview](../tools/overview.md#addressing-a-symbol-by-name).

## What's Next

See [everyday scenarios](scenarios.md) for prompts that combine these tools into real tasks.
