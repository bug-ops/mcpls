# Security and Trust

In this chapter you learn what mcpls trusts, what it does not, and how to analyze code you did not write without surprises. The short version: a language server executes code that comes with the workspace, mcpls does not sandbox it, and the controls below narrow the exposure without removing it.

The authoritative policy, including how to report a vulnerability, is [SECURITY.md](https://github.com/bug-ops/mcpls/blob/main/SECURITY.md) in the repository. This chapter explains it in practical terms.

## Prerequisites

- You know [where configuration comes from](../getting-started/minimal-config.md#where-the-configuration-file-lives).

## Two kinds of trust

1. **Trust in the mcpls configuration.** A `mcpls.toml` names the commands mcpls spawns, their environment and their options. Running a command from a file you did not write is code execution.
2. **Trust in the workspace.** Even with a safe configuration, language servers run code that the analyzed project supplies. This is inherent to LSP and the same for every LSP bridge.

mcpls controls the first directly and narrows the second.

## Project-local configuration is ignored by default

A `./mcpls.toml` in the current directory is not loaded unless you pass `--trust-project-config` (or set `MCPLS_TRUST_PROJECT_CONFIG=true`). Without it mcpls logs a warning naming the ignored file and falls through to your user config or the built-in defaults, which still start servers by project markers.

Naming a path is consent, so `--config <path>` and `MCPLS_CONFIG` are always trusted. A repository that exports `MCPLS_CONFIG=./mcpls.toml` from a tool such as direnv therefore loads it automatically. That is by design, but worth knowing when you audit a checkout.

Trusting a project config also lets it write the agent-facing `title`, `description` and `instructions` text, with nothing marking it as not coming from mcpls itself.

`--trust-project-config` is a grant for the whole mcpls process, not for one project. Set it on a per-project client entry, not in your shell profile or a user-wide client config.

## Language servers run workspace code

| Server | Workspace-supplied code it can run |
|--------|------------------------------------|
| typescript-language-server | The workspace's `node_modules/typescript/lib/tsserver.js` unless pinned; tsconfig plugins; automatic type acquisition may fetch packages over the network |
| rust-analyzer | Cargo build scripts and procedural macros |
| pyright, pylsp | The project's Python environment, plugins and interpreters named in config |
| gopls | The Go toolchain, including toolchain downloads requested by `go.mod` |
| clangd | Commands from `compile_commands.json` and `.clangd` configuration |

Only the typescript-language-server row has been verified live; the others describe the well-known behavior of those servers.

Pointing mcpls at an untrusted checkout, such as a cloned third-party repository or a pull-request branch, can therefore run that checkout's code. Do it only in an environment you are willing to have that code execute in, such as a container or a disposable VM.

## What mcpls does to narrow the exposure

- **Cleared environment.** A server starts with an empty environment plus an allowlist (`PATH`, `HOME`, `USERPROFILE`, `TMPDIR`, `TEMP`, `TMP`, and the Windows system variables), and then the entry's `env`. Variables such as `NODE_OPTIONS` or `LD_PRELOAD` are not passed on unless you set them.
- **Workspace boundary.** Tool calls reject paths outside every workspace root, and results flag locations outside the roots. Rename and code-action edits that target files outside the roots are withheld and counted in `dropped`.
- **Secret redaction.** Secret-looking values are removed from text that reaches logs and clients ([Diagnostics and Resources](diagnostics.md#redaction)).
- **tsserver pin.** The TypeScript server is pointed at its own bundled `tsserver` instead of the workspace's ([TypeScript](typescript.md)).
- **No authentication by design.** mcpls itself authenticates no one on any transport; see [Transports](transports.md#there-is-no-authentication).
- **Untrusted-workspace mode.** A command-line mode that starts only servers you name and adds executable, configuration and environment checks ([Untrusted-Workspace Mode](untrusted-mode.md)).

## Practical guidance

| Situation | Recommendation |
|-----------|----------------|
| Your own project | Defaults are fine |
| A repository you trust but did not write | Review its `mcpls.toml` before using `--trust-project-config` |
| A repository you do not trust | Use a container or VM, and consider [untrusted mode](untrusted-mode.md) |
| A team-shared client config | Remember that a project-scoped `.mcp.json` is controlled by the repository |
| HTTP on a non-loopback address | Always put an authenticating reverse proxy in front |

## What's Next

Next, read how [untrusted-workspace mode](untrusted-mode.md) enforces boundaries you can rely on.
