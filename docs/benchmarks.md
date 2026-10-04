# Benchmarks

`mcpls-bench` (crate `crates/mcpls-bench`, `publish = false`) measures mcpls
latency end to end: it spawns a real `mcpls` process, speaks MCP over stdio, and
lets mcpls drive a real language server on an open-source repository pinned to an exact commit.
Every timed call is checked for a correct answer, so a fast wrong result is
recorded as `incorrect`, not as a good sample.

Unix only (macOS, Linux). Windows is not supported.

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

Requirements per scenario: `fd-rust-analyzer` needs `git`, `cargo` and
`rust-analyzer`; `react-hook-form-tsls` needs `git`, `pnpm`, `node` and
`typescript-language-server` (with `typescript`).

Useful flags: `--runs` (default 3), `--warmup-runs` (1), `--iterations` (5 per
probe per run), `--ready-timeout-secs` (300), `--call-timeout-secs` (60),
`--work-dir` (relative paths are resolved against the current directory),
`--allow-version-mismatch`.

## Methodology

- **Fresh process per run.** Each run spawns mcpls, which spawns the language
  server, and shuts both down afterwards. mcpls leads its own process group.
  Shutdown is bounded (10 s for mcpls to exit, then up to 1 s, polled every
  50 ms, for the rest of the group to follow) and recorded as `clean`,
  `killed` (mcpls had to be killed, or the session did not close cleanly) or
  `orphans_killed` (mcpls exited but members of its group, such as cargo or
  proc-macro servers, outlived it and were killed). `orphans_killed` takes
  precedence over `killed`: it does not say whether the session had closed cleanly.
- **Interruption.** During `run`, `Ctrl-C` or `SIGTERM` kills the whole process
  group of the active run before exiting with 130 or 143, so no cargo or rustc
  of the run is left running to skew the next one. During `prepare` only the
  direct child (git, a setup command) is killed, so a `SIGTERM` can leave
  grandchildren of a setup step running; an interrupted clone leaves at most a
  `repos/.<name>.partial` directory, which the next `prepare` wipes.
- **Process-tree limits.** Descendants that detach with `setsid` escape the
  group kill and the RSS sum. A zombie member whose parent is still alive counts
  as a live member until it is reaped, which can report `orphans_killed` for a
  harmless leftover. Windows has no process group here, so only mcpls itself is
  killed. Two concurrent `prepare` runs of one scenario are not supported (they
  share the `.partial` staging directory).
- **mcpls stderr** goes to
  `<work-dir>/logs/<scenario>/<invocation unix millis>/run-<index>.log`; the
  path is recorded per run as `stderr_log`, and later invocations never
  overwrite it. The files are not size-limited.
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
  skipped.
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
  measured runs; failed, incorrect and timed-out samples are counted separately.
  `p95_us` is the nearest-rank observed value and is `null` (printed as `-`)
  below 20 samples, where it would equal the maximum. With the defaults a probe
  has 12 steady samples, so p95 appears only with more `--runs` or `--iterations`.
- **Process memory (RSS)** is sampled with `ps` at two checkpoints, `ready`
  (right after the ready probe passed) and `after_probes`, never during a call.
  The `ready` reading adds one `ps` call (tens of milliseconds of idle time)
  between the ready probe and the first probe, during which the server may keep
  indexing, so `first` latencies are slightly flattered compared with no
  sampling. `after_probes` is skipped for a run truncated by a timeout.
  Each reading lists every member of the mcpls process group and their sum
  (`memory` per run, `memory_summary` min / median / max of the sums). Limits:
  the sum is per process group, so pages shared between processes are counted
  more than once, and descendants that detach with `setsid` (for example
  rust-analyzer flycheck, which mcpls does not trigger because it sends no
  `didSave`) are missed. RSS is `unavailable` where `ps` is not usable.

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
  `PATH` entries are ignored), then their version
  command runs with the repository as the working directory (mcpls spawns the
  server there, so a rustup proxy picks the same toolchain). The resolved path
  and version output are recorded in the report (`mcpls`, `server`, `runtime`).
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
scenarios and repositories you trust. Reports contain absolute local paths
(including your home directory) and tool versions; review them before
publishing.

## Push-only servers

typescript-language-server (5.1.3) does not advertise pull diagnostics, so
`react-hook-form-tsls` has no diagnostics probe. Add `diagnostics` probes only
for servers that support pull diagnostics (rust-analyzer does).

## Adding a scenario

Create a TOML file under `crates/mcpls-bench/scenarios/`:

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

[[runtime]]                              # recorded toolchains, optional
command = "cargo"
version_args = ["--version"]

[ready_probe]
kind = "hover"                           # hover | definition | references | document_symbols | diagnostics
file = "src/main.rs"                     # relative to the repo root
position = { line = 63, character = 18 } # 1-based
contains = "ExitCode"

[[probes]]
kind = "definition"
file = "src/main.rs"
position = { line = 63, character = 18 }
uri_suffix = "src/main.rs"
```

Expectations per kind: `hover.contains`, `definition.uri_suffix`,
`references.min_count`, `document_symbols.symbol`,
`diagnostics.expect = { kind = "no_errors" }` or
`{ kind = "at_least", count = N }`.

## Output schema

The JSON report is `mcpls_bench::report::RunReport`: `scenario`, `source`
(`pinned` commit or `unpinned` path), `mcpls`/`server`/`runtime` pin records,
`mcpls_binary`, `params`, `runs` (each with `warmup`, `ready`,
`truncated_after_timeout`, `samples`, `memory`, `stderr_log`, `shutdown`),
`aborted`, `summary` (per region: `ok`, `not_ok`, `first` and `steady`
min/median/p95/max) and `memory_summary`. A sample is
`{ region, outcome, elapsed_us, iteration }` where `outcome` is `ok`,
`incorrect { detail }`, `failed { error }` or `timed_out`.

## Smoke test

`crates/mcpls-bench/tests/smoke.rs` runs the harness once against the in-repo
Rust fixture (no network, no setup) and asserts that every sample is `ok`. It is
`#[ignore]`d because it needs a built `mcpls` and `rust-analyzer`; the e2e CI job
runs it. It keeps the harness in step with changes to MCP tool output shapes.
