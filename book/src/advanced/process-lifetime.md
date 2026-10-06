# Process Lifetime

In this chapter you learn what happens to language server processes when mcpls exits, whether normally or not, and the platform caveats. Language servers can start heavy children such as `cargo check` or build daemons, and a bridge that leaks them would slowly eat your machine.

## Prerequisites

- You know how servers [start and stop](lifecycle.md).

## The guarantee

When mcpls exits for any reason, including `SIGKILL` and out-of-memory kills, the language servers and the descendants that stay in their process tree are killed. This includes processes that are meant to outlive their server, such as shared build daemons like Gradle or Bloop that the server started.

The mechanism differs by platform.

## Unix

Each server has its own lifeline: a small anchor process leads the server's process group, and a watchdog sits outside the group. When mcpls exits, or the server is shut down or restarted, the watchdog freezes the tree, finds descendants that left the group with `setsid()` or `setpgid()` (for example the `cargo check` that rust-analyzer's flycheck runs), and kills everything. A crashed server's leftovers are killed when the crash is noticed.

Caveats:

- The sweep needs `ps` and `awk`. Without `ps`, mcpls logs a warning and escaped descendants survive.
- Descendants whose parent exited before the sweep (reparented to init) cannot be attributed and may survive. A `pkill -9` that matches `lsp-lifeline-watchdog` kills the watchdog and prevents the sweep.
- Servers that exit only on stdin EOF wait out the 3 second shutdown grace and are then killed. `processId` is sent as `null` in `initialize`.
- Servers run in their own process group, so Ctrl-C in the terminal no longer reaches them directly. A descendant that reads `/dev/tty`, such as an ssh or git credential prompt, may be stopped by `SIGTTIN` when mcpls runs in an interactive terminal.
- If the anchor cannot be started, the server runs unbound in its own process group. If only the watchdog fails, the group is killed from mcpls on exit but escaped descendants are not swept. Ctrl-C does not reach servers in either case.

## Windows

Servers run in a job object without breakaway, and the whole tree is killed. A descendant that requests `CREATE_BREAKAWAY_FROM_JOB` fails to spawn. If the job cannot be created or assigned, the server spawn fails.

## Why this matters to you

- You can kill mcpls or your client at any moment without leaving `rust-analyzer` or its children behind.
- Shared daemons started by a server do not survive it. If a server needs a daemon that outlives it, start that daemon yourself, outside mcpls.
- `restart_server` uses the same machinery: it kills the whole tree before starting a replacement.

## What's Next

Finish Part 3 with [Limits and Known Constraints](limits.md), a checklist of the boundaries to plan around.
