# Copilot Instructions for mcpls

mcpls is a bridge between MCP (Model Context Protocol) and LSP (Language Server Protocol).
AI clients speak MCP to mcpls; mcpls spawns and communicates with language servers over LSP.

```
AI Client ←→ [MCP/stdio] ←→ mcpls ←→ [LSP/stdio] ←→ language server
```

Key crates: `mcpls-core/src/bridge/`, `lsp/`, `mcp/`, `config/`.

## Type safety

Prefer newtypes over primitive aliases for domain values. A raw `u32` for a line number
and a raw `u32` for a column are indistinguishable to the compiler; a `Line(u32)` and
`Column(u32)` make transpositions a compile error.

Encode protocol state in the type system where possible. A document that has been opened
and one that has not should ideally be different types, not the same type with a boolean
field. Typestate prevents calling operations that are only valid on open documents.

Avoid stringly-typed dispatch. Matching on `&str` method names to branch protocol
behaviour should be replaced with a typed enum as soon as the set of methods is known
and bounded.

`Option<T>` and `Result<T, E>` must not be unwrapped with `.unwrap()` or `.expect()` in
production code paths. Use `?`, `if let`, `let-else`, or explicit error mapping.

## Idiomatic Rust

Prefer iterator adapters over manual loops. `map`, `filter`, `flat_map`, `collect`, and
`fold` express intent more clearly and are easier to compose than `for` with a mutable
accumulator.

Avoid unnecessary heap allocation. Prefer `&str` over `String` in function signatures
where ownership is not required. Prefer `Cow<str>` when a function sometimes borrows and
sometimes owns.

Use `let-else` (stabilised in Rust 1.65) to reduce nesting when early return on `None`
or `Err` is needed:
```rust
// prefer
let Some(value) = option else { return Err(...) };
// over
let value = match option { Some(v) => v, None => return Err(...) };
```

Derive `Clone`, `Debug`, `PartialEq` on data types unless there is a specific reason not
to. Their absence makes testing harder and the omission is rarely intentional.

## Architecture

A component that bridges two protocols (MCP and LSP) must not let either protocol's
failure mode affect the other. LSP request timeouts must not stall MCP response
delivery, and LSP push notifications must not block MCP request handling.

Shared mutable state must be scoped as narrowly as possible. A `Mutex` that guards a
large composite struct is a concurrency bottleneck; prefer separate locks for
independent sub-components (e.g. a cache and a client are independent).

A `Mutex` guard must never be held across an `.await` point that involves I/O. Doing so
holds the lock for the full duration of the network round-trip, blocking every other
task that needs the lock. Extract the guarded value before the `.await` or restructure
so the lock is released first.

## Async

Prefer `tokio::join!` or `tokio::try_join!` over sequential `.await` chains when
operations are independent. Sequential awaits serialise work that could run concurrently.

Blocking operations (`std::fs`, `std::thread::sleep`, CPU-heavy loops) must not run on
the async executor. Use `tokio::fs`, `tokio::time::sleep`, or `tokio::task::spawn_blocking`
for work that would block a task for more than a few microseconds.

Use `tokio::sync::mpsc` for async-to-async channels. Use `std::sync::mpsc::sync_channel`
only for bridging a synchronous context to async, and always with `try_send` when the
intent is to drop events under backpressure — `send` blocks on a full channel.

Prefer `tokio::select!` with a cancellation token over bare `loop { recv().await }`
for background tasks that must shut down cleanly.

## API currency and MSRV

When reviewing code, check whether newer stable Rust APIs would simplify it. If a
simpler or safer API was stabilised after the current `rust-version` in `Cargo.toml`,
suggest bumping MSRV and using the new API. Document the bump in CHANGELOG under
`### Changed`.

Examples of APIs worth adopting when MSRV permits:
- `let-else` expressions (1.65) — replace verbose `match`/`unwrap_or_else` for early return
- `std::io::read_to_string` on a `File` handle (1.0, but pairing with `.metadata()` on the same
  handle closes TOCTOU windows — prefer over separate `stat` + `open`)
- `OnceLock` (1.70) — prefer over `lazy_static` or `once_cell` for static initialisation
- `is_some_and` / `is_none_or` (1.70) — replace `map(|v| ...).unwrap_or(false)` patterns
- `Iterator::array_chunks` (nightly → stable tracking) — watch stabilisation status

When a dependency provides a feature already in `std`, prefer `std`. Fewer dependencies
reduce supply-chain risk and compile time.
