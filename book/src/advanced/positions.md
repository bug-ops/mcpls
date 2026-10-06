# Positions and Encodings

In this chapter you learn how mcpls converts positions between MCP and LSP, and why a result can carry `positions_degraded`. If you only use ASCII source files, you can skip it; it matters for text with accents, CJK characters or emoji.

## Prerequisites

- You know the `line` and `character` arguments ([Tools Overview](../tools/overview.md#addressing-code)).

## Two coordinate systems

| | MCP tools (what the assistant sees) | LSP (what language servers use) |
|---|---|---|
| First line | 1 | 0 |
| First column | 1 | 0 |
| Column unit | UTF-16 code units | The encoding negotiated with the server |

All conversions go through one module, so the off-by-one adjustment is never repeated in a tool.

## Why encoding matters

LSP counts the columns of a line in "code units", and servers may differ in which unit they use. Take the line `let s = "héllo";`. The character `é` is one character, two bytes in UTF-8 and one code unit in UTF-16. A column after it differs by one between UTF-8 and UTF-16.

For ASCII text all encodings agree. They diverge after any non-ASCII character on the same line.

## Negotiation

During the `initialize` handshake, mcpls offers the encodings in `workspace.position_encodings` in order. The default is `["utf-8", "utf-16"]`:

```toml
[workspace]
position_encodings = ["utf-8", "utf-16", "utf-32"]
```

The list must be non-empty and contain only `utf-8`, `utf-16` and `utf-32`. It is a preference: UTF-16 is the mandatory fallback in the LSP specification, so a server may answer with UTF-16 even if you omit it.

## Converting to the server's encoding

When the server does not use UTF-16, mcpls converts each column using the text of the line, taken from the open document or read from disk. Reading from disk is bounded by a per-response budget of four times `workspace.max_file_size` (four times the default when it is `0`), at most 256 MiB.

When a column cannot be converted exactly, the result is still returned and is marked `positions_degraded`:

| Value | Meaning | What to do |
|-------|---------|------------|
| `"request"` | The position you sent reached the server unconverted, so the result may describe a different symbol | Do not trust the result |
| `"response"` | Only returned `character` offsets may be inexact; the result describes the symbol you asked about | Keep the result, treat columns as approximate |

Causes include an exhausted disk-read budget, an unresolvable path reported by the server, an oversized file, invalid UTF-8, a line past the end of the file, and a column inside a multi-unit character. Column 1 and columns past the end of a line never trigger the flag; the latter clamp to the end of the line.

## What's Next

Next, see how the language server processes themselves are started, watched and stopped: [Language Server Lifecycle](lifecycle.md).
