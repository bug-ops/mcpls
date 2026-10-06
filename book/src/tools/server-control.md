# Server Monitoring and Control

In this chapter you learn the four tools that look at the language servers themselves: what they say, what they can do, and how to restart one that has gone wrong. Reach for them when a tool answers oddly or not at all.

## Prerequisites

- You know the [tool conventions](overview.md).

## get_tool_support

Reports which tools are usable for which languages in the current session, without making a call that is certain to fail. Call it before using a tool on a language you have not used yet.

Argument: optional `file_path`, which restricts the report to that file's language.

```json
{ "file_path": "/work/app/src/main.rs" }
```

Per tool, `coverage` is `all`, `some`, `none`, `unknown` (a server is still initializing) or `always` (needs no language server). `routes` groups languages by `status`:

| `status` | Meaning |
|----------|---------|
| `supported` | The call will be dispatched to a server that advertises the capability |
| `push_only` | The server publishes diagnostics but has no pull provider, so `get_diagnostics` answers from the push cache |
| `capability_not_advertised` | The server does not advertise the feature |
| `initializing` | The server has not finished starting |
| `no_server` | No server is routed for this tool |

`supported` means the call will be dispatched, not that it will succeed: indexing, push-only diagnostics and respawn backoff can still fail it.

## get_server_logs

Returns recent log messages from the language servers. Arguments: `limit` (default 50) and `min_level`, exactly one of `error`, `warning`, `info` or `debug` in lowercase.

```json
{ "limit": 20, "min_level": "warning" }
```

Use it when completion or hover fails, to see messages such as a project that could not be loaded.

## get_server_messages

Returns the user-facing messages servers sent with `window/showMessage`, such as status updates and prompts. Argument: `limit` (default 20).

```json
{ "limit": 10 }
```

## restart_server

Restarts language servers: it stops the old process (a graceful shutdown, then a kill of its whole process group) and starts a fresh one, discarding its in-memory state. Use it when a server is wedged or serves a stale index, for example after editing `Cargo.toml` or `package.json`.

Give either `servers` (a list of server ids, taken from `name` or `language_id`) or `all: true`.

```json
{ "servers": ["rust"] }
```

Per server, `status` is:

| `status` | Meaning |
|----------|---------|
| `restarted` | A new process runs; `indexing_state` is `unknown`, `loading` or `ready` |
| `failed` | The restart failed with a typed `reason`; the server stays registered and the next tool call retries |
| `throttled` | Restarted too recently; retry after `retry_in_ms` |
| `initializing` | The server is still starting |
| `not_running` | The server never started; fix the cause and restart mcpls |

This tool is destructive: it kills processes the server started, including daemons other clients may share. It is neither read-only nor idempotent. Requests in flight on the old process fail with the retryable `-32054` error, and the first whole-workspace query afterwards may wait while the new server indexes.

## What's Next

If something does not work as described, go to [Troubleshooting](../guide/troubleshooting.md).
