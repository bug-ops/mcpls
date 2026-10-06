# TypeScript: tsserver Pinning and TypeScript 7

In this chapter you learn how mcpls keeps TypeScript analysis from running the workspace's own `tsserver`, and how to use the TypeScript 7 native server. TypeScript is the one language where the choice of server executable is subtle, so it gets its own chapter.

## Prerequisites

- You have read [Security and Trust](security.md).
- `typescript-language-server` is installed ([Language Servers](../guide/language-servers.md)).

## The problem

`typescript-language-server` finds a `tsserver` to do the real analysis. By default it prefers the one in the workspace's `node_modules/typescript`, which is workspace-supplied code that can load tsconfig plugins.

## The pin

By default mcpls passes the `tsserver` bundled next to `typescript-language-server` as `initializationOptions.tsserver.path`, so the workspace's own copy is not selected. The pin applies to these installs, each with a `typescript` package whose `package.json` has a `version`, next to the server:

- `npm -g` style symlink installs;
- npm `.cmd`, `.ps1` and extensionless shims beside a `node_modules` that holds the server;
- pnpm global installs, found under `<PNPM_HOME>/global`;
- `node` or `bun` running the server's absolute `cli.mjs`.

Shims are located by file existence and are never executed or parsed.

### When the pin is not applied

mcpls logs a warning naming the reason, and the server runs unpinned, in these cases:

| Case | What to do |
|------|------------|
| Server not found on `PATH` | Install `typescript-language-server` or fix `command` |
| Started through a Volta, asdf or mise shim, or another wrapper script | Set `initialization_options.tsserver.path` to a trusted `tsserver.js` |
| Started through `npx`, `bunx`, `pnpm dlx`, `yarn dlx`, `deno npm:`, or `node`/`bun` with a relative script path | Use the `typescript-language-server` executable or an absolute `cli.mjs`, or set `tsserver.path` |
| A pnpm global install with more than one `global/<version>` entry holding the server, or more than 16 entries | Remove stale entries, or set `tsserver.path` |
| No valid `typescript` package next to the server | `npm install -g typescript@6` |
| Only TypeScript 7 or later next to the server, which ships no `tsserver` | Install `typescript@6` next to the server, or use the native server below |
| You set `initialization_options` without `tsserver.path` | Add `tsserver.path` to keep the pin |
| A warning after startup that the server reports a version source other than `user-setting` | The pin did not take effect, for example after a restart with a stale pin |

A `tsserver.path` you set always wins. Set it to a workspace path to opt back in to the workspace's TypeScript:

```toml
[[lsp_servers]]
language_id = "typescript"
command = "typescript-language-server"
args = ["--stdio"]
file_patterns = ["**/*.ts", "**/*.tsx"]

[lsp_servers.initialization_options]
tsserver.path = "/opt/typescript/lib/tsserver.js"
```

The pin is resolved once per start, and again on respawn when the pinned path no longer resolves to the same file. A server installed inside the workspace is pinned, but that narrows nothing, because the server itself is workspace code. Automatic type acquisition network fetches are not prevented. The pin narrows one vector; it does not make an untrusted workspace safe. Volta, asdf, mise and package-runner launchers are not covered.

## TypeScript 7 and the native server

TypeScript 7 is the native port of the compiler. Its npm package ships no `lib/tsserver.js`, so `typescript-language-server` cannot use it. You have three options.

### Option 1: keep the JavaScript server

Install a JavaScript-based TypeScript next to it:

```bash
npm install -g typescript-language-server typescript@6
```

Installing `typescript@6` globally replaces a global TypeScript 7 `tsc`. To keep both, install TypeScript 7 into a separate prefix, for example `npm install --prefix ~/ts7 typescript@7`.

### Option 2: let mcpls choose

The generated default `typescript` entry carries `selection = "auto"`. Add the key to an existing entry to opt in; it is valid only on a `typescript-language-server` command.

```toml
[[lsp_servers]]
language_id = "typescript"
command = "typescript-language-server"
args = ["--stdio"]
file_patterns = ["**/*.ts", "**/*.tsx"]
selection = "auto"
```

At startup mcpls then starts the native server, `tsc --lsp --stdio`, when TypeScript 7 is installed outside every workspace root and no JavaScript `tsserver` next to `typescript-language-server` can be pinned. Details:

- It looks next to `typescript-language-server`, then for `tsc` on `PATH`. Only a `typescript/bin/tsc` of a TypeScript 7 install is accepted, never one inside the workspace (symlinks included).
- Otherwise `typescript-language-server` is kept, also when `tsc` needs `node` and `node` is not on `PATH`. The choice and its reason are logged at info level.
- A TypeScript 6 install next to `typescript-language-server` wins over a TypeScript 7 one.
- An `initialization_options.tsserver.path` you set always keeps `typescript-language-server`.
- When the native server is chosen, mcpls replaces the entry's `command` and `args`. Remove `selection` to keep your own.
- On Windows the npm or pnpm `tsc.cmd` shim is mapped to its `typescript` package and started as `node <package>\bin\tsc --lsp --stdio`, never through `cmd.exe`, with the first `node.exe` on `PATH`. If that `node` or its directory lies inside a workspace root, the TypeScript language server is kept. This launch has not been verified on a live Windows install, so Option 3 is the safe choice there.

### Option 3: run the native server explicitly

Edit the existing `typescript` entry of your config file (or remove it first) so that its `command` and `args` are as below, and remove its `selection` key, because `selection = "auto"` is rejected on any other command:

```toml
[[lsp_servers]]
language_id = "typescript"
command = "/home/me/ts7/node_modules/.bin/tsc"
args = ["--lsp", "--stdio"]
file_patterns = ["**/*.ts", "**/*.tsx"]
```

Use the absolute path of a TypeScript 7 install outside the workspace: `<npm prefix>/bin/tsc` (`<npm prefix>\tsc.cmd` on Windows). A `tsc` from the workspace's `node_modules`, or a bare `tsc` that `PATH` may resolve into the workspace, is workspace-supplied code that mcpls would run, and the pin does not apply to it.

Do not add this as a second `typescript` entry next to the default one. Two entries for one language that both omit `handles` are rejected at startup, with a "duplicate server id" error when both are unnamed and with a "two catch-all servers" error even when the new one has its own `name`.

The native server reports diagnostics through pull requests only, does not support type hierarchy, and loads no tsconfig plugins.

## What's Next

Next, see how mcpls makes sure no language server process outlives it: [Process Lifetime](process-lifetime.md).
