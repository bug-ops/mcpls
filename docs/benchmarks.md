
Both targets pin only the top-level package or commit. Their transitive
dependencies (Python packages resolved by `uvx`, npm packages resolved by `npx`)
float, so two runs on different days can differ in more than the pinned version.
# Benchmarks

`mcpls-bench` (crate `crates/mcpls-bench`, `publish = false`) measures mcpls
latency end to end: it spawns a real `mcpls` process, speaks MCP over stdio, and
lets mcpls drive a real language server on an open-source repository pinned to an exact commit.
Every timed call is checked for a correct answer, so a fast wrong result is
recorded as `incorrect`, not as a good sample. The same scenarios can be run
against comparison MCP servers (Serena, lsmcp) through a target definition.

Unix (macOS, Linux) is the supported platform. Windows builds and uses a Job
Object for the process tree and PowerShell/CIM for memory, but that path is
compile-checked only: it is not run in CI and has not been exercised on a
Windows machine.

> **Status:** no performance claims are made in the README. Numbers are
> published only after they are reproduced on a second machine. Results are not
> committed to this repository.

## Quick start

```bash
cargo build --release -p mcpls -p mcpls-bench

# Untimed: clone the pinned commit and run the scenario's setup steps
target/release/mcpls-bench prepare crates/mcpls-bench/scenarios/fd-rust-analyzer.toml

# Timed
target/release/mcpls-bench run crates/mcpls-bench/scenarios/fd-rust-analyzer.toml \
    --output fd-result.json
```

`run` prints the full JSON report to stdout and a summary table to stderr.
`--mcpls` selects the mcpls binary; by default it is the `mcpls` next to
`mcpls-bench`, never one found on `PATH`; a `--mcpls` value is always a file
path, so a bare `mcpls` means `./mcpls`. Rebuild `mcpls` yourself
(`cargo build -p mcpls`) before a run: building `mcpls-bench` does not rebuild
it. The report records the binary's path, version, inferred build profile
(`debug`/`release` from its directory name), size and modification time.

See [Scenarios](#scenarios) for the toolchain each scenario needs.

Useful flags: `--runs` (default 3), `--warmup-runs` (1), `--iterations` (5 per
probe per run), `--ready-timeout-secs` (300), `--call-timeout-secs` (60),
`--work-dir` (relative paths are resolved against the current directory),
`--allow-version-mismatch`, `--stderr-log-max-mib` (32), `--target` (a
comparison server instead of mcpls, see [Comparison targets](#comparison-targets)).

## Methodology

- **Fresh process per run.** Each run spawns mcpls, which spawns the language
  server, and shuts both down afterwards. mcpls leads its own process group (a
  Job Object on Windows). Shutdown is bounded: 10 s for mcpls to exit, then up
  to `LIFELINE_SWEEP_BUDGET` + 1 s (9 s) for the rest of its tree to follow,
  polled every 200 ms. The budget is that of the per-server watchdogs, which
  sweep the servers' process groups after mcpls exits, so a watchdog that is
  still sweeping is not mistaken for a leak. The outcome is `clean`, `killed`
  (mcpls had to be killed, or the session did not close cleanly) or
  `orphans_killed` (mcpls exited but members of its tree outlived the budget
  and were killed). `orphans_killed` takes precedence over `killed`: it does not
  say whether the session had closed cleanly.
- **Process tree.** Since the language servers sit in their watchdog's process
  group, and helpers such as rust-analyzer's flycheck call `setsid`, group
  membership alone no longer covers them. The harness reads the whole process
  table (`ps -A` on Unix, `Get-CimInstance Win32_Process` on Windows) and takes
  the parent-pid closure of mcpls: that is what RSS sums and what shutdown
  checks. Every process seen at a checkpoint (and once more right before
  shutdown) is remembered by **pid plus start time** (`lstart` on Unix,
  `CreationDate` on Windows), so a survivor is a process with the same pid and
  the same start time, never merely the same pid. Survivors are killed on Unix
  after re-verifying that identity against a fresh table; nothing is ever killed
  by a bare pid from an old snapshot. On Windows a parent link counts only if
  the parent started before the child (the recorded parent pid goes stale and is
  recycled), nothing is killed by pid, and the Job Object kills the tree.
- **Interruption.** `Ctrl-C` or `SIGTERM` drops the active process-group guard,
  which kills the group of the active run before exiting with 130 or 143.
  `prepare` runs its clone and setup steps in their own process group too, so an
  interrupted `pnpm install` or `cargo check` leaves no grandchild behind; an
  interrupted clone leaves at most a `repos/.<name>.partial` directory, which
  the next `prepare` wipes. Descendants that left the group are swept by the
  servers' watchdogs when mcpls dies, not by the harness.
- **Process-tree limits.** A zombie is ignored. A descendant whose parent died
  before a sample is not in the closure (it has been reparented), so RSS can miss
  a helper that outlived its parent between checkpoints; it is still found by
  identity at shutdown if it was seen earlier. The PowerShell sampling used on
  Windows costs hundreds of milliseconds per checkpoint.
- **Concurrency.** An exclusive lock on `<work-dir>/locks/<scenario>.lock` is
  held for the whole of `prepare` and `run` (`flock` on Unix, a zero share mode
  on Windows, retried for about 2 s there so an antivirus scan does not look
  like a second holder). A second invocation for the same scenario fails fast,
  because `prepare` stages in the shared `repos/.<name>.partial` directory and
  `run` shares configs and logs. Different scenarios do not block each other.
- **mcpls stderr** is piped through a capped copy into
  `<work-dir>/logs/<scenario>/<invocation unix millis>/run-<index>.log`. The
  first `--stderr-log-max-mib` MiB (default 32) are written; the rest is read and
  dropped so mcpls never blocks on a full pipe, and a truncation marker is
  appended. After shutdown the copy gets 2 s to see end-of-stream and is then
  abandoned. The report records `stderr_log` per run as `{ path,
  written_bytes, dropped_bytes, drain_complete, error }`; later invocations
  never overwrite earlier logs.
- **Timed vs untimed.** Cloning, `cargo fetch`, `cargo check` (warms `target/`
  and proc macros) and `pnpm install` happen in `prepare` and are never timed.
- **Regions.**
  - `startup`: spawn until the MCP `initialize` handshake completes.
  - `ready`: spawn until the scenario's `ready_probe` first passes.
  - `hover`, `definition`, `references`, `document_symbols`, `diagnostics`:
    latency of one tool call, repeated `--iterations` times per run.
- **Readiness is semantic.** `ready_probe` is a normal probe with an expected
  answer (for example hover text containing a symbol name). It is retried every
  50 ms until it passes (`params.ready_retry_interval_ms`); the attempt count is recorded. If it never passes
  within `--ready-timeout-secs` the `ready` sample is `timed_out`, the last failing
  attempt is kept in `ready.last_failure`, and the run's remaining probes are
  skipped. A comparison target that cannot answer the scenario's ready probe
  uses the first probe it can answer instead.
- **Fail fast.** Permanent errors during readiness (invalid params such as a
  wrong file path or position outside the file, a closed transport) end the
  wait immediately. Every other failure is retried, and the last one is kept in
  the report (`ready.last_failure`). If a run never becomes ready, the remaining
  runs are skipped and the report is marked `aborted`.
- **Warm-up runs** (`--warmup-runs`, default 1) are kept raw, marked
  `"warmup": true`, and excluded from the summary. The first run also warms the
  disk (page cache, `target/`), so results are warm-disk numbers.
- **First iteration vs steady state.** Every probe runs `--iterations` times per
  fresh process. Iteration 0 pays one-off costs (document open, lazy
  initialisation) and is summarised separately (`first`); iterations 1 and up
  (`steady`) are warm latencies. Set `--iterations` to at least 2 to get a
  steady-state figure. `startup` and `ready` occur once per run and are
  reported under `first`.
- **Timeouts.** After a call times out, mcpls is still serving it, so later
  calls would queue behind it and be inflated. The remaining probes of that run
  are skipped and the run is marked `truncated_after_timeout`.
- **Summary** is min / lower median / p95 / max over successful samples of the
  measured runs; failed, incorrect, unsupported and timed-out samples are counted
  separately (`not_ok`, except unsupported ones, which get their own `unsupported` count so a capability gap does not look like a regression). `p95_us` is the nearest-rank observed value and is
  `null` (printed as `-`) below 20 samples, where it would equal the maximum.
  With the defaults a probe has 12 steady samples, so p95 appears only with more
  `--runs` or `--iterations`.
- **Process memory (RSS)** is sampled from the process table at two checkpoints,
  `ready` (right after the ready probe passed) and `after_probes`, never during
  a call. The `ready` reading adds one process-table read (tens of milliseconds
  of idle time) between the ready probe and the first probe, during which the
  server may keep indexing, so `first` latencies are slightly flattered compared
  with no sampling. `after_probes` is skipped for a run truncated by a timeout.
  Each reading lists every member of the mcpls process tree (see above) and
  their sum (`memory` per run, `memory_summary` min / median / max of the sums).
  Pages shared between processes are counted more than once. RSS is
  `unavailable` where the process table cannot be read or mcpls is no longer in
  it.

## Pinning and reproducibility

Only the repository commit is enforced. Tool versions (language server,
toolchains, mcpls) are recorded in the report, and enforced only when a scenario
opts in with `expected_version`; none of the bundled scenarios does.

- Repositories are pinned to a full commit SHA (`RepoSource::Git`). `prepare`
  fetches exactly that commit and refuses a checkout at any other commit; `run`
  re-checks `git rev-parse HEAD` against the pin and reports the observed commit.
- `prepare` writes a marker (outside the repository) after all `setup` steps
  succeed. `run` refuses to start without a marker matching the scenario's
  current setup steps, so untimed work (`cargo fetch`, `pnpm install`) cannot
  leak into the timed `ready` region.
- Executables are resolved to an absolute path via `PATH` (relative and empty
  `PATH` entries are ignored; on Windows `PATHEXT` extensions are tried, so
  `pnpm` finds `pnpm.cmd`), then their version command runs with the repository
  as the working directory (mcpls spawns the server there, so a rustup proxy
  picks the same toolchain). A server without a version flag of its own names a
  separate `version_command` (pyright-langserver uses `pyright --version`). The
  resolved path and version output are recorded in the report (`target`,
  `runtime`).
- A scenario may set `expected_version` (a substring of the version output) per
  executable. A mismatch aborts the run unless `--allow-version-mismatch` is passed; the
  report's pin record then shows the mismatch (`version_output` does not contain
  `expected_version`).
- The work directory (default `<cache dir>/mcpls-bench`) must not be inside a
  project: an ancestor `Cargo.toml` makes cargo treat the clone as a workspace
  member (rust-analyzer would fail to load it), and an ancestor
  `pnpm-workspace.yaml` or `node_modules` changes how pnpm and TypeScript
  resolve it. The ancestors are checked before anything is created, so a
  refused path leaves no directory behind.
- Clones are staged in `repos/.<name>.partial` and renamed into place after
  HEAD verifies, so an interrupted clone never looks complete. A leftover
  incomplete checkout from an older version (a `.git` without a resolvable HEAD)
  is removed and cloned again; a checkout at a wrong commit still aborts.
- Git is hardened: scenario URLs must be `https://github.com/<owner>/<repo>`
  in canonical form (no credentials, port, query, fragment, whitespace,
  backslash, `..` or upper-case host, so git and the URL parser read it the
  same way). Every git call removes every inherited `GIT_*` environment
  variable, ignores user and system configuration
  (`GIT_CONFIG_GLOBAL` is the null device, so `insteadOf` rewrites and a global
  `http.proxy` do not apply; proxies set through environment variables still
  do), allows only the https protocol, and puts `--` before positional
  arguments of `remote add` and `fetch`.
- `RepoSource::Local` (used only by the smoke scenario) is unpinned and reported
  as such.

## Trust model

A scenario is code. `setup` commands run as your user, and `cargo check`,
rust-analyzer and `pnpm install` execute build scripts and proc macros of the
pinned repository (TypeScript scenarios use `--ignore-scripts`). Run only
scenarios and repositories you trust. A comparison target is code too: its
definition names the command that is launched. Reports contain absolute local
paths (including your home directory) and tool versions; review them before
publishing.

## Scenarios

Every scenario pins its repository to a full commit. `Validated` means the
scenario was run end to end on the machine that wrote it; the others have only
had their probe positions and expected answers checked against the pinned
sources, because the language server was not installed there.

| Scenario | Repository | Server | Needs | Validated |
|---|---|---|---|---|
| `fd-rust-analyzer` | sharkdp/fd 10.5.0 | rust-analyzer | `cargo`, `rust-analyzer` | not re-run here |
| `react-hook-form-tsls` | react-hook-form 7.69.0 | typescript-language-server | `pnpm`, `node`, `typescript-language-server`, `typescript` | yes |
| `react-hook-form-tsgo` | react-hook-form 7.69.0 | `tsgo --lsp --stdio` | `pnpm`, `node`, `tsgo` (`@typescript/native-preview`) | no |
| `httpx-pyright` | encode/httpx 0.28.1 | pyright-langserver | `pyright` | probes only (see below) |
| `httpx-ty` | encode/httpx 0.28.1 | `ty server` | `ty` | no |
| `cobra-gopls` | spf13/cobra 1.10.2 | gopls | `go`, `gopls` | no |
| `fmt-clangd` | fmtlib/fmt 12.2.0 | clangd | `clangd`; setup writes `compile_flags.txt` (no CMake) | yes |
| `zls-zig-args` | MasterQ32/zig-args | zls | `zig`, `zls` of matching versions | no |
| `mcp-typescript-sdk-tsls` | modelcontextprotocol/typescript-sdk 2.3.0 | typescript-language-server | `pnpm`, `node`, `typescript-language-server` | no |
| `vscode-tsls` | microsoft/vscode 1.140.0 | typescript-language-server | `npm`, `node`, `typescript-language-server`; several GB of disk | no |
| `smoke-fixture` | in-repo fixture | rust-analyzer | `rust-analyzer` | yes |

Notes:

- `vscode-tsls` and `mcp-typescript-sdk-tsls` are the scale scenarios. Run
  `vscode-tsls` with a long `--ready-timeout-secs` (for example 1800) and on
  dedicated hardware; it is not part of the scheduled workflow.
- `httpx-pyright`: through mcpls every request to pyright-langserver 1.1.408
  times out after 60 s, so the scenario is unvalidated end to end. The cause is
  an existing mcpls bug, not the harness: mcpls advertises
  `workspace.workspaceFolders` but never sends `workspace/didChangeConfiguration`,
  so pyright waits for a configuration push and blocks every request until
  shutdown. A raw client that sends `workspace/didChangeConfiguration` with
  `{ settings: null }` after `initialized` gets the hover in 0.5 s. The scenario
  stays blocked on that fix (#578).
- `fmt-clangd`: clangd reports out-of-line members under their qualified name,
  so the symbol probe asks for `buffered_file::close`.
- The `symbol` field of a hover, definition or references probe is the
  identifier at the position. mcpls ignores it; comparison targets that address
  symbols by name use it.
- `cobra-gopls` and `zls-zig-args` carry no `diagnostics` probe, because
  pull-diagnostics support of those servers has not been verified.

## Comparison targets

`--target <file>` replaces mcpls with another MCP server that is driven by the
same scenario (repository, probes, readiness). The target brings its own
language servers, so the scenario's `server` is neither used nor pinned; the
report's `target` is `external` and records the launcher's path and version, the
pinned version and `verification: textual`.

A target definition (`crates/mcpls-bench/targets/*.toml`) names:

- `pinned`: a version or commit that must appear in a launch argument, so the
  launch cannot drift (checked when the file loads);
- `launcher` (the executable, version-recorded) and `args`, where `"{repo}"` is
  replaced by the absolute repository path;
- `cleanup`: repository-relative paths the target writes. They are removed before
  and after every run, so one run never sees another's state;
- `[tools.<kind>]` for `hover`, `definition`, `references`, `document_symbols`
  and `diagnostics`: the MCP `tool`, its `arguments`, and for `references` and
  `diagnostics` a `count_marker`. An argument value is a constant or exactly one
  placeholder: `{repo}`, `{file}`, `{relative_file}`, `{line1}`, `{line0}`,
  `{character1}`, `{character0}`, `{symbol}`. Anything else in braces is an
  error.

A probe kind without a binding, or one needing a position or symbol the probe
lacks, is recorded as `unsupported` (one sample, no call). Right after startup
`tools/list` must contain every bound tool, or the run fails at once.

Answers are checked as text: `hover.contains`, `definition.uri_suffix` and
`document_symbols.symbol` as substrings, `references.min_count` and the
`diagnostics` expectations as occurrences of `count_marker`. That is weaker than
the typed checks used for mcpls, so a comparison says "answered with the
expected substring", not "answered identically".

| Target | Launch | Pin | Notes |
|---|---|---|---|
| `serena` | `uvx --from git+https://github.com/oraios/serena@<sha> serena start-mcp-server` with the `ide` context | v1.7.0 | web dashboard, browser and GUI log window are switched off by flags; `.serena/` is cleaned before and after every run; no hover tool, so `hover` is `unsupported` |
| `lsmcp` | `npx -y @mizchi/lsmcp@0.10.0 -p typescript` | 0.10.0 | `.lsmcp/` is cleaned; the reference count is approximate (`.ts:` occurrences); TypeScript scenarios only |

Both were run once against `react-hook-form-tsls` on the machine that wrote
them, to check the flow; no numbers are kept or published. lsmcp did not exit
on its own after stdin closed, so those runs end `killed`.

## Scheduled run

`.github/workflows/bench.yml` runs weekly (Sunday 04:00 UTC) and on demand
(`workflow_dispatch`) on `ubuntu-latest` with read-only permissions: it builds
release binaries, runs `prepare` and `run` for `fd-rust-analyzer` and
`react-hook-form-tsls` (5 runs, 1 warm-up, 10 iterations), and uploads each JSON
report as a workflow artifact for 30 days. Nothing is published or committed.
Shared CI runners are noisy: use these reports to spot regressions in memory and
shutdown behaviour, not to quote latencies.

## Not done yet

Out of scope for the change that introduced the items above; tracked for a
follow-up:

- reproduction of any result on a second machine;
- published results and a speed section in the README;
- validated end-to-end runs of the scenarios marked "no" above (their language
  servers were not installed where they were written) and of `httpx-pyright`
  through mcpls;
- scheduled runs on Windows and macOS, and a `vscode-tsls` scale run on
  dedicated hardware;
- published Serena and lsmcp comparison results, and running the Windows path
  on a Windows machine.

## Push-only servers

typescript-language-server (5.1.3) does not advertise pull diagnostics, so the
TypeScript scenarios have no diagnostics probe. Add `diagnostics` probes only
for servers that support pull diagnostics (rust-analyzer does).

## Adding a scenario

Create a TOML file under `crates/mcpls-bench/scenarios/`; its `name` must equal
the file stem:

```toml
name = "my-scenario"                     # [a-z0-9-]+, also the clone directory
setup = [{ command = "cargo", args = ["fetch"] }]

[source]
kind = "git"                             # or "local" with `path`
url = "https://github.com/owner/repo"
commit = "<40-char sha>"

[server]
language_id = "rust"
file_patterns = ["**/*.rs"]
args = []

[server.executable]
command = "rust-analyzer"
version_args = ["--version"]
expected_version = "rust-analyzer 1.99"  # optional
# version_command = "pyright"            # optional: print the version with another command

[[runtime]]                              # recorded toolchains, optional
command = "cargo"
version_args = ["--version"]

[ready_probe]
kind = "hover"                           # hover | definition | references | document_symbols | diagnostics
file = "src/main.rs"                     # relative to the repo root
position = { line = 63, character = 18 } # 1-based
contains = "ExitCode"
symbol = "ExitCode"                      # optional, for comparison targets

[[probes]]
kind = "definition"
file = "src/main.rs"
position = { line = 63, character = 18 }
uri_suffix = "src/main.rs"
```

Expectations per kind: `hover.contains`, `definition.uri_suffix`,
`references.min_count`, `document_symbols.symbol`,
`diagnostics.expect = { kind = "no_errors" }` or
`{ kind = "at_least", count = N }`. A references probe should sit on a symbol
that is defined in its file, because comparison targets look references up by
the file that defines the symbol.

## Output schema

The JSON report is `mcpls_bench::report::RunReport`: `scenario`, `source`
(`pinned` commit or `unpinned` path), `target` (`mcpls` with `binary`, `build`
and `server`, or `external`), `runtime` pin records, `params`, `runs` (each with
`warmup`, `ready`, `truncated_after_timeout`, `samples`, `memory`, `stderr_log`,
`shutdown`), `aborted`, `summary` (per region: `ok`, `not_ok`, `unsupported`, `first` and
`steady` min/median/p95/max) and `memory_summary`. A sample is
`{ region, outcome, elapsed_us, iteration }` where `outcome` is `ok`,
`incorrect { detail }`, `failed { error }`, `timed_out` or `unsupported`.

## Smoke test

`crates/mcpls-bench/tests/smoke.rs` runs the harness once against the in-repo
Rust fixture (no network, no setup) and asserts that every sample is `ok`. It is
`#[ignore]`d because it needs a built `mcpls` and `rust-analyzer`; the e2e CI job
runs it. It keeps the harness in step with changes to MCP tool output shapes.
