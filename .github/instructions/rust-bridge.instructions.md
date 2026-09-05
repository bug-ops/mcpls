---
applyTo: "crates/mcpls-core/src/bridge/**,crates/mcpls-core/src/lsp/**,crates/mcpls-core/src/lib.rs"
---

## Type safety

Position types (line, column) must be distinct newtypes, not bare `u32`. The MCP
convention (1-based) and the LSP convention (0-based) must be expressed in the type
system so that passing an MCP position directly to an LSP call is a compile error.

Protocol message variants must be an enum, not a string-matched dispatch. Matching on
`"textDocument/hover"` strings is fragile; a closed enum of known methods makes
unhandled cases visible at compile time and exhaustiveness-checked.

`InboundMessage` and similar enums that may gain variants in future protocol versions
must be `#[non_exhaustive]` so downstream crates are not broken by additions.

## Document state

Caching document content alongside a filesystem signature (`mtime` + size) prevents
stale reads after external edits. On every access, stat the file and compare against
the cached signature; re-read and re-notify the LSP server on mismatch.

Stat and read must use the same file handle to close the TOCTOU window. Open the file
once with `tokio::fs::File::open`, call `.metadata().await` on the handle, then read
through the same handle. A separate `tokio::fs::metadata` call before `read_to_string`
leaves a window where an atomic `rename` can change the file between the two calls.

Document version numbers must be strictly monotone per URI. On overflow, reset to 1
rather than saturating — after `didClose`/`didOpen` the server treats the document as
fresh and the version value is irrelevant to continuity.

## Diagnostics pipeline

When a feature draws from multiple independent sources (pull request + push cache),
fetch them concurrently with `tokio::try_join!` or `tokio::join!`. Awaiting them
sequentially serialises work that has no data dependency.

A cache miss must be distinguishable from an empty result. Returning `Vec::new()` for
both "no errors" and "not yet analysed" is ambiguous to callers. Use an `Option<Vec>`
or a dedicated status type to express the difference.

Deduplication keys should prefer structured fields (error code) over free-form message
text. When a code is present, key on `(range, severity, code)`. The doc comment must
describe the actual key — a mismatch between the comment and the implementation causes
silent behavioural bugs.

## Concurrency and lock scope

Background notification pumps must hold only the narrowest lock needed (e.g. a
`Mutex<Cache>`, not a `Mutex<WholeTranslator>`). A pump that shares a lock with request
handlers will stall when a handler holds the lock across a long-running await, causing
the notification channel to fill and the in-flight response to be undeliverable.

Never hold a `MutexGuard` across an `.await`. Extract needed values before the await,
or restructure so the guard is dropped before any I/O begins.

## File watcher

Glob patterns from external registrations must be matched against full absolute paths.
`globset` does not implicitly anchor bare patterns to a directory prefix — a pattern
like `*.rs` only matches a bare filename. Prepend `**/` to patterns that lack a leading
`/` or `**/` before compiling them into a `GlobSet`.

Noise filtering (build artifacts, VCS directories) must run before events enter any
bounded channel, not after. Filtering post-send wastes channel capacity and can stall
the OS watcher thread when the channel fills during high-churn builds.

Use `try_send` for the handoff from the OS watcher callback to the async processing
loop. The watcher callback runs on a system thread; blocking it with `send` on a full
channel stalls the OS-level event delivery.

## LSP client

Server-to-client requests (messages with both `method` and `id`) require a JSON-RPC
response. Unhandled methods must return error code `-32601` (Method Not Found). Dropping
them silently causes the LSP server to wait indefinitely for an acknowledgement.

`id`-only messages (no `method`, `result`, or `error`) are protocol errors and must not
be coerced into a successful `null` response.
