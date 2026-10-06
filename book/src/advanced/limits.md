# Limits and Known Constraints

In this chapter you find every boundary mcpls enforces or cannot cross, in one place. Use it to plan a deployment, and to recognize a limit when a result looks cut short.

## Prerequisites

- Familiarity with the [configuration](../guide/configuration.md) and the [tool conventions](../tools/overview.md).

## Resource limits you can configure

| Limit | Default | Where to change it |
|-------|---------|--------------------|
| Open documents | 100 (least recently used closed first) | `workspace.max_documents` |
| Largest file opened | 10 MiB (at most 1 GiB) | `workspace.max_file_size` |
| Servers starting at once | 8 | `workspace.max_concurrent_server_starts` |
| Wait for indexing | 30 s (above 3, below 60) | `workspace.indexing_ready_timeout_seconds` |
| Handshake timeout | 30 s (1 to 900) | `timeout_seconds` |
| Request timeout | 30 s (1 to 900) | `request_timeout_seconds` |
| Project-marker search depth | 10 (at most 64) | `workspace.heuristics_max_depth` |
| `mcp.title`, `description`, `instructions`, `tool_prefix` | 128, 1024, 4096 and 32 bytes | `[mcp]` |

## Limits that are fixed

| Limit | Value |
|-------|-------|
| Completion request | 10 s, whatever `request_timeout_seconds` says |
| Retries on "content modified" | 4 attempts, 3.5 s of total backoff |
| `shutdown` request to a server | 5 s |
| Child exit grace after shutdown | 3 s |
| `restart_server` cooldown | 5 s per server |
| Respawn backoff | 1 s growing to 30 s |
| Server ids per `restart_server` call | 64 |
| Diagnostics cache | 1000 files |
| Server log buffer, server message buffer | 100 and 50 entries |
| Subscriptions per session | 1000 |
| Concurrent `subscriptions/listen` streams | 100 |
| Enclosing-symbol enrichment | 16 files (or `max_documents / 4` when `max_documents` is below 64), 30 s |
| Per-response disk read for position conversion | 4 times `max_file_size`, at most 256 MiB |
| Result lists | Capped per tool; `truncated: true` marks a cut |

## Behavioral constraints

- **mcpls does not write your files.** Edits are returned for the client to apply.
- **Routing is by extension.** `file_patterns` cannot confine a server to a directory, and two servers that claim the same extension both receive every file with it unless `handles` separates them.
- **`file_patterns` forms.** Only `*.EXT` (with an optional `**/` prefix) and bare extensionless names such as `Makefile` are accepted. Brace expansion, character classes, `?`, dotfiles, dotted names, single files and multi-part extensions such as `*.tar.gz` are rejected at startup.
- **`workspace_symbol_search` has no document.** It goes to the first server that claims `workspace_symbols`, else the first catch-all, and fails if there is none.
- **A configuration file replaces the built-in server list.** A file with no `[[lsp_servers]]` starts no servers.
- **Same-size restores are invisible.** A file restored with an identical size and modification time is not seen as changed; see [Troubleshooting](../guide/troubleshooting.md#results-look-stale-after-a-file-changed).
- **Position conversion can degrade** for non-UTF-16 servers ([Positions and Encodings](positions.md)).
- **Not every server supports every tool.** Use `get_tool_support`.
- **Servers see a reduced environment.** Restore variables through `env`.

## Transport constraints

- **HTTP is opt-in at build time** (`transport-http`) and absent from prebuilt binaries and the Docker image.
- **HTTP/1 only.** No HTTP/2.
- **No authentication on any transport.** Use loopback or an authenticating reverse proxy.
- **No wildcards** in the Host and Origin allowlists.

## Security constraints

- mcpls does not sandbox language servers; an allowed server runs workspace code.
- The tsserver pin does not cover version-manager shims, package runners or relative-script wrappers.
- Untrusted mode does not check interpreter arguments or launchers that choose the server from workspace files ([Untrusted-Workspace Mode](untrusted-mode.md#what-it-does-not-cover)).
- Redaction does not cover URIs, diagnostic `data` keys, or tool results such as hover text ([Diagnostics and Resources](diagnostics.md#redaction)).
- Platform notes: the Windows launch of the native TypeScript server and the Windows `USERPROFILE` replacement were verified less than the Unix paths. Where a Windows layout is covered only by unit tests, prefer explicit absolute paths.

## What's Next

For a lookup of every flag and key, go to the [Command Line and Environment](../reference/cli.md) and [Configuration Reference](../reference/config.md) pages.
