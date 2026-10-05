# Security Policy

## Supported versions

Only the latest released minor version of `mcpls` receives security fixes.

## Reporting a vulnerability

Report vulnerabilities privately through GitHub private vulnerability reporting:
open the repository's **Security** tab and choose **Report a vulnerability**
(`https://github.com/bug-ops/mcpls/security/advisories/new`). Do not file a
public issue for an undisclosed vulnerability.

Include the affected version, the language server involved, and a minimal
reproduction. Reports are acknowledged on a best-effort basis.

## Trust model

`mcpls` bridges an AI client to language servers that analyze a workspace.
Analyzing a workspace is not a safe operation for an untrusted workspace, and
`mcpls` does not sandbox the servers it spawns.

- **Workspace-supplied `mcpls` config.** A project-local `./mcpls.toml` can name
  server commands and environment, and set per-server `settings` and
  `initialization_options`, which can steer what a server executes (for example
  pyright's interpreter or rust-analyzer's `check.overrideCommand`). It is ignored unless you pass
  `--trust-project-config`. This flag governs only the `mcpls` config. It does
  not make the workspace itself safe to analyze.
- **Language servers execute workspace code.** This is inherent to LSP and the
  same for every LSP bridge. Pointing `mcpls` at an untrusted checkout (a cloned
  third-party repository, a pull-request branch) can run that checkout's code
  through the servers `mcpls` auto-spawns:

  | Server | Workspace-supplied code the server can run |
  |--------|---------------------------------------------|
  | typescript-language-server | `node_modules/typescript/lib/tsserver.js` from the workspace, unless pinned (see below); tsconfig plugins when the workspace tsserver runs (a pinned tsserver does not load them from the workspace); automatic type acquisition may fetch packages over the network |
  | rust-analyzer | Cargo build scripts and procedural macros |
  | pyright / pylsp | The project's Python environment, plugins, and interpreters named in config |
  | gopls | The Go toolchain, including toolchain downloads requested by `go.mod` |
  | clangd | Commands from `compile_commands.json` and `.clangd` configuration |

  Only the typescript-language-server row has been verified live. The other rows
  describe the well-known behavior of those servers.

- **tsserver pin.** By default `mcpls` passes the tsserver bundled next to
  typescript-language-server as `initializationOptions.tsserver.path`, so the
  workspace's own tsserver is not selected. The pin applies to `npm -g` style
  symlink installs (verified with Homebrew's node) that have a global
  `typescript` package, with a `package.json` carrying a `version`, next to the
  server. It is not applied, and a warning is logged, when:
  - the server is started through a Windows `.cmd` shim or a script launcher
    (pnpm, Volta, asdf/mise);
  - the server is started through `npx`, `bunx`, `node cli.mjs` or another
    wrapper that only names it in the arguments;
  - no valid `typescript` package is found next to the server;
  - you set `initialization_options` for the server without `tsserver.path`.

  A server installed inside the workspace is pinned, but that pins the
  workspace's own `typescript` and the server itself is workspace code, so
  nothing is narrowed. A `tsserver.path` you set yourself always wins; set it
  to a workspace path to opt back in to the workspace's TypeScript. The pin is
  resolved once at startup. The pin narrows one vector. It does not make an
  untrusted workspace safe.

- **Automatic native `tsc` selection.** An entry with `selection = "auto"` (the
  generated TypeScript default) may start `tsc --lsp --stdio` from a TypeScript 7
  install. mcpls accepts only a `typescript/bin/tsc` that canonicalizes to a path
  outside every workspace root, so a workspace `node_modules` is never selected. The
  native server loads no tsconfig plugins (checked live). This
  does not make an untrusted workspace safe. Remove `selection` to run `command` as
  written.

- **Explicitly configured workspace binaries.** A server `command` you configure,
  for example a TypeScript 7 `tsc` for the native server (`tsc --lsp --stdio`),
  is run as written. A `tsc` from the workspace's `node_modules`, or one that
  `PATH` resolves into the workspace, is workspace-supplied code. The tsserver pin
  does not apply to it.

An untrusted-workspace mode is tracked in #603. Run `mcpls` against untrusted code only inside an environment you are willing to
have that code execute in (a container or a disposable VM).
