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
  workspace's own tsserver is not selected. The pin applies to these installs,
  each with a `typescript` package, with a `package.json` carrying a `version`,
  next to the server:
  - `npm -g` style symlink installs (verified with Homebrew's node);
  - npm `.cmd`, `.ps1` and extensionless shims beside a `node_modules` that holds
    the server (verified live only for the Unix shell shim; the Windows layouts
    are covered by unit tests);
  - pnpm global installs (verified live on macOS; the Windows layout is covered
    by unit tests), found under `<PNPM_HOME>/global`, where more than one match
    or more than 16 entries is ambiguous and not pinned;
  - `node` or `bun` running the server's absolute `cli.mjs`.

  Shims are located by file existence only and are never executed or parsed.
  On Windows a bare command is looked up as written, so an install that only
  provides `typescript-language-server.cmd` needs that name as `command`.
  It is not applied, and a warning is logged, when:
  - the server is started through Volta, asdf, mise or another version-manager
    shim;
  - the server is started through `npx`, `bunx`, `pnpm dlx`, `yarn dlx`,
    `deno npm:` or another wrapper that only names it in the arguments, including
    `node` or `bun` with a relative script path;
  - no valid `typescript` package is found next to the server;
  - you set `initialization_options` for the server without `tsserver.path`.

  Version-manager shims are not resolved because their install directory can
  only be found through the manager's own layout, and asdf, mise and Volta pick
  the version from workspace files; set `initialization_options.tsserver.path`
  to pin them. In untrusted-workspace mode the first two cases are refused
  instead of started with a warning (see below).

  A server installed inside the workspace is pinned, but that pins the
  workspace's own `typescript` and the server itself is workspace code, so
  nothing is narrowed. A `tsserver.path` you set yourself always wins; set it
  to a workspace path to opt back in to the workspace's TypeScript. The pin is
  resolved once per start, and the path that is checked against the workspace
  is the canonical path that is sent. A respawn resolves it again when the
  pinned path no longer resolves to the same file, with the same check in
  untrusted mode. The pin narrows one vector. It does not make an
  untrusted workspace safe.

- **Automatic native `tsc` selection.** An entry with `selection = "auto"` (the
  generated TypeScript default) may start `tsc --lsp --stdio` from a TypeScript 7
  install. mcpls accepts only a `typescript/bin/tsc` that canonicalizes to a path
  outside every workspace root, so a workspace `node_modules` is never selected. On
  Windows the npm `tsc.cmd` shim is mapped to its package and the server starts as
  `node <package>\bin\tsc --lsp --stdio`, never through `cmd.exe`. That `node` is the
  first `node.exe` on the server's effective `PATH` and passes the same check: if it
  or the `PATH` directory it was found in lies inside a workspace root, the
  TypeScript language server is kept instead. The native server loads no tsconfig
  plugins (checked live). This does not make an untrusted workspace safe. Remove
  `selection` to run `command` as written.

- **Explicitly configured workspace binaries.** A server `command` you configure,
  for example a TypeScript 7 `tsc` for the native server (`tsc --lsp --stdio`),
  is run as written. A `tsc` from the workspace's `node_modules`, or one that
  `PATH` resolves into the workspace, is workspace-supplied code. The tsserver pin
  does not apply to it.

## Untrusted-workspace mode

`--workspace-trust untrusted` starts only the language servers you name with
`--allow-server <id>` (repeatable; the id is the server's `name`, else its
`language_id`, such as `rust`). Every other applicable server is refused before
it is spawned, restarted or respawned, and a tool call routed to it returns an
error that names the server and the flag that would start it. The flags are
command-line only: there is no environment variable and no config key, so a
config file planted in the workspace cannot grant consent. The mode conflicts
with `--trust-project-config` (also when set by `MCPLS_TRUST_PROJECT_CONFIG`),
and `--allow-server` without it is a usage error (exit code 2).

This is not a sandbox. An allowed server still runs the workspace code listed
above, so allow a server only when you accept that. The mode additionally
enforces the following, and nothing more:

- **Config file.** The file that was actually loaded must lie outside the
  workspace: `--config`, `MCPLS_CONFIG` and the auto-discovered user config
  (found through `$HOME` or `$XDG_CONFIG_HOME`, which a checkout's tooling can
  set) are all checked. It is checked against the configured roots (or the
  working directory when none are configured). The working directory is also
  checked when the path is relative or came from the environment. A working
  directory that is `/` or your login home directory (taken from the account
  database, never from `$HOME`, which a checkout's tooling can set) is never
  treated as a checkout.
  No default config file is created in this mode.
- **Executable.** The server's executable is resolved the way a spawn would
  find it and must exist outside the workspace roots; one that cannot be
  resolved is refused. mcpls then spawns that resolved absolute path (also on
  restart and respawn, so a binary added to a workspace directory later is not
  picked up) with a `PATH` that has the workspace, relative and empty entries
  removed. A server always gets such a `PATH`, even when mcpls itself has none,
  and a fixed system path if nothing is left, since an empty or missing `PATH`
  makes shells and `execvp` search the current directory. A `#!/usr/bin/env node` interpreter, and any tool the
  server itself looks up on `PATH`, therefore cannot resolve into the
  workspace through `PATH`.
- **Environment.** Servers start with a cleared environment. What is passed
  on from mcpls's own is `PATH` (sanitized as above), `HOME`, `USERPROFILE`,
  `TMPDIR`, `TEMP`, `TMP` and the Windows system variables. `NODE_OPTIONS`,
  `LD_PRELOAD`, `DYLD_INSERT_LIBRARIES`, `PYTHONPATH`, `RUSTC_WRAPPER` and
  `XDG_CONFIG_HOME` are not passed unless the server's own `env` sets them. In
  this mode `HOME` and `USERPROFILE` are replaced by the login home directory
  (from the account database) unless the server's `env` sets them, so a
  `$HOME` that names the workspace cannot steer rustup, cargo or npm
  configuration; mcpls itself also rejects a config file reached through such
  a `$HOME` or `$XDG_CONFIG_HOME`. If the login home cannot be determined (or
  is not valid UTF-8), a server whose inherited `HOME` or `USERPROFILE` lies
  inside the workspace, or is empty (tools resolve an empty home against the
  working directory, which is the checkout), is refused, and so is an unset
  `HOME` on Unix (`~` would resolve against the working directory); only an
  existing value outside the workspace is passed on unchecked. The `USERPROFILE` replacement was not
  verified on Windows. Where `$HOME` legitimately differs from the account home
  (CI container jobs with `HOME=/github/home`, `sudo`, Nix or Bazel sandboxes),
  servers see the account home, so toolchains installed under the overridden
  `$HOME` are not found: set `HOME` in that server's `env` to opt in.
- **tsserver pin.** A TypeScript server whose pinned tsserver lies inside the
  workspace is refused instead of started, and so is one started through a
  launcher no tsserver can be pinned for: a package runner, a version-manager
  shim, or a wrapper that only names the server in its arguments (also when you
  set `tsserver.path`, since the launcher still picks the server).
- **Launcher.** A `command` that lets the workspace choose the program is
  refused: package runners (`npm`, `npx`, `bunx`, `pnpm`, `pnpx`, `yarn`, `uvx`,
  `corepack`, `deno npm:`), task runners (`make`, `just`, `task`, `rake`, `mvn`,
  `sbt`), programs that run their arguments (`xargs`, `find`, `awk`, `gawk`,
  `mawk`, `nawk`, `script`, `su`, `flock`, `watch`), wrappers whose options are not
  analyzed (`sudo`, `sudo-rs`, `doas`, `run0`, `pkexec`, `runuser`, `setpriv`,
  `gosu`, `su-exec`, `chpst`, `setuidgid`, `envdir`, `runas`, `wsl`, `strace`,
  `unshare`, `chrt`, `taskset`, `ionice`, `chroot`, `nsenter`, `systemd-run`:
  they change the user, root, directory or environment, or take optional
  arguments) and the run subcommands of `bun`, `deno` (including `eval` and `repl`),
  `cargo`, `go`, `uv`, `pipx`, `poetry`, `pdm`, `hatch`, `bundle` and `dotnet`. `npx` runs
  `./node_modules/.bin/<name>` from the working directory before anything else
  and reads a workspace `.npmrc`, so the planted package would run. `env` is
  unwrapped; `env -S`, `env -P`, a `PATH=` assignment and a relative program that `env -C`
  itself starts cannot be analyzed and are refused (a program a nested wrapper
  starts is resolved from mcpls' working directory and spawned by its absolute
  path). A command string
  cannot be analyzed either, so it is refused for these shells (`-c`, `--command`,
  `--commands`, `/c`, `/k`, `/r`, PowerShell's `-Command`, `-CommandWithArgs`
  (`-cwa`) and `-EncodedCommand`, fish's `-C` and `--init-command`, nushell's
  `-e` and `--execute`): `sh`, `bash`, `zsh`, `dash`, `ash`, `hush`, `ksh`, `mksh`,
  `oksh`, `yash`, `posh`, `fish`, `csh`, `tcsh`, `elvish`, `nu`, `xonsh`, `cmd`,
  `powershell`, `pwsh` (also as a `busybox` applet, and with a version suffix such
  as `ksh93`), and for these interpreters given an inline program (`-e`, `-E`,
  `-c`, `-p`, `-r`, `--eval`, `--print`, also with the value glued on or after
  `=`): `node`, `nodejs`,
  `bun`, `python`, `perl`, `ruby`, `php`, `lua`, `rscript`, `julia`, `osascript`;
  also a perl `-M` or `-m` value that is more than a module name and an import
  list (`-MPOSIX;code`), and a `data:` URL given to `--import`, `--loader` or
  `--experimental-loader` of `node`, `nodejs` and `bun`.
  The wrappers with a small option grammar (`time`, `nice`, `nohup`, `timeout`,
  `setsid`, `stdbuf`, `caffeinate`, `arch`, and `env`, also under their Homebrew GNU
  names `gtime`, `gnice`, `gnohup`, `gtimeout`, `gstdbuf`, `genv` and as `busybox`,
  `toybox` or `coreutils` applets, the last as `coreutils --coreutils-prog=NAME`
  or, for uutils, the bare applet name; `--coreutils-prog-shebang=` is refused) are parsed against a closed table of their options: the first
  non-option after the options (and `timeout`'s duration) is the program they
  start, and what follows it belongs to that program. An option the table does
  not list, a missing value or command, and nesting deeper than 8 levels are
  refused and the refusal names the option (never its value). Each program a
  wrapper starts is then resolved, refused when it lies inside the workspace,
  and replaced by its absolute path, as the server's own executable is. `deno lsp`
  is allowed. These lists are closed, not exhaustive: a shell, interpreter or
  wrapper that is not named here (`numactl`, `prlimit`, `ltrace`, `valgrind`,
  `firejail`, `sg`, `run-parts`, `xcrun`, `sandbox-exec`) is admitted without
  unwrapping it, and its command string is unexamined. Whichever launcher
  precedes it, though, a server is refused when an argument of its `command`
  (or the text after the first `=` of one, as in `--exec=<path>`), resolved
  against the directory the server starts in with symlinks followed, is an
  executable file inside the workspace; a file without an execute bit (on
  Windows, without a program extension), a directory and text that is no file
  are admitted, and so is a program given by bare name, which the sanitized
  `PATH` already keeps out of the workspace. An argument that lies inside the
  workspace and cannot be read (permission denied) is refused as well. An
  executable data file passed in `args` is therefore refused. A resolved wrapped-program path
  that contains `=` is refused, because `env` would read it as an assignment. The list matches the command's
  file stem and arguments and is best-effort: the trusted configuration is the
  boundary, so install the server globally and give its absolute path as
  `command`.
- **Working directory.** The server starts in your login home directory (else
  the system temporary directory when no other user can write to it, which
  rules out a shared `/tmp`), never in the checkout, and is refused when
  neither is available outside the workspace. Servers get the workspace from
  `workspaceFolders`, so a server that treats its working directory as the
  workspace root, or a relative path in `args`, no longer resolves into the
  checkout; give such a server absolute paths and `workspace.roots`.
- **Windows executable lookups.** `NoDefaultCurrentDirectoryInExePath=1` is
  always set in the server's environment (a value you set is overridden), and
  the working directory above is outside the workspace, so `cmd.exe` running an
  npm `.cmd` shim, and a Node server starting `python` or `git` by name, do
  not find a `node.exe`, `node.cmd` or `node.bat` in the checkout. Verified on
  Windows only by the CI test that starts a `.cmd` server and records its
  working directory.

What the mode does not cover:

- Interpreter arguments that are not executable files: `node
  <workspace>/cli.mjs` runs workspace code, and only the `node` executable is
  checked, unless `cli.mjs` has an execute bit (any executable file named in
  `args` is refused, see Launcher).
- With no configured `workspace.roots`, the working directory is the checkout
  unless it is `/` or the login home. With no account entry, or a `$HOME` that
  differs from it and equals the working directory, binaries under it (such as
  `~/.cargo/bin`) and a config found there are refused: configure
  `workspace.roots`.
- Directories above a configured root, such as a monorepo around the
  configured package, count as outside the workspace.
- Case-insensitive file systems and hardlinks are not specifically handled or
  tested: a hardlinked binary has no distinguishable location.
- On Windows the standard library also searches the application directory and
  the system directories; mcpls resolves only the child `PATH`, so a command it
  cannot resolve is refused rather than guessed.
- The mode is not surfaced to MCP clients through `get_info`; the refusals are
  in the error text of the affected tool calls and in the log.
- `TMPDIR`, `TEMP` and `TMP` are passed on as inherited, and values a
  server's own `env` sets are used as written. When the login home is
  unknown, a `HOME` set to the parent of a configured root (roots
  `[<ws>/pkg]`, `HOME=<ws>`) is not refused: it falls under the directories
  above a configured root listed below.
- Launchers that choose the real server from files in the workspace and are not
  on the launcher list: rustup honors a workspace `rust-toolchain.toml` whose
  `path` names a toolchain inside it, so the `rust-analyzer` proxy outside the
  workspace can run a binary from the workspace; asdf, mise and Volta shims
  (refused only for the TypeScript server) pick versions from workspace files;
  Go switches toolchains from `go.mod`. The executable check sees only the
  launcher, and the launcher list is best-effort.
- A server restarted or respawned runs the path resolved at startup. If that
  path goes through a symlink inside the workspace, the symlink can be
  repointed later; only the resolved path's own directory is checked, not
  every link of a chain.
- TypeScript pin: a pin that resolves inside the workspace is refused, as is a
  launcher that cannot be pinned. A server that is pinnable but has no valid
  `typescript` next to it is admitted without a pin. mcpls starts the server
  without a `rootUri`, so it does not look up a workspace tsserver on its own,
  which is why this is not treated as a bypass.
- Code the allowed servers run on their own (build scripts, procedural macros,
  tsconfig plugins) is outside every check above.

A project-scoped MCP client config (for example a `.mcp.json` in the analyzed
repository) controls the arguments mcpls is launched with, so it can drop
`--workspace-trust untrusted` or add `--allow-server`. Set the mode in your
user-scoped client configuration.

Run `mcpls` against untrusted code only inside an environment you are willing to
have that code execute in (a container or a disposable VM).
