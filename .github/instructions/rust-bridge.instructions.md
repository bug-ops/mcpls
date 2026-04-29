---
applyTo: "crates/mcpls-core/src/bridge/**,crates/mcpls-core/src/lsp/**,crates/mcpls-core/src/lib.rs"
---

## Document state

Lazy document opening must re-verify on-disk state on every access, not only on first
open. Cache a filesystem signature (e.g. `mtime` + file size) alongside the document
state and re-read the file when the signature changes. On mismatch, send `didClose`
then `didOpen` with a bumped version before serving the request.

Stat and read must be bound to the same file handle to avoid TOCTOU. Performing a stat,
then separately opening the file for reading, creates a window where an atomic replace
(`rename`) can change the content without changing the signature.

Version numbers must remain strictly monotone per URI. On counter overflow, reset to 1
— after `didClose`/`didOpen` the server treats the document as a fresh open regardless
of the version value.

## Diagnostics

When a feature can source results from multiple channels (e.g. pull request and push
cache), the channels should be fetched concurrently rather than sequentially. Blocking
a cheap synchronous lookup behind a slow network round-trip wastes time.

An empty result must be distinguishable from an absent result. Returning an empty
collection for "server has not yet analysed this file" is indistinguishable from "no
errors found" to callers. Either surface a cache-miss indicator or fall back to a
live request on cache miss.

Deduplication keys should prefer structured fields (e.g. error code) over free-form
message text. When a code is present, key on `(range, severity, code)`. Fall back to
a normalised message only when no code exists. Document the actual key in the doc
comment — mismatches between the comment and the implementation cause silent bugs.

## Notification pump

A background notification pump must hold only the narrowest lock needed to write to
the cache. It must not share a lock with request handlers that hold it across network
I/O. If pump and handler compete for the same mutex, the handler's in-flight network
response can be blocked from arriving — the system deadlocks until the request timeout
fires.

## File watcher

Glob patterns from LSP registrations must be matched against full absolute paths.
`globset` does not implicitly anchor bare patterns to any directory. A pattern like
`*.rs` matches only a filename with no path component. Prepend `**/` to bare patterns
before compiling them into a glob set. Example:

```rust
// wrong: "*.rs" won't match "/repo/src/lib.rs"
// correct: "**/*.rs" matches any .rs file at any depth
let anchored = if !pattern.starts_with('/') && !pattern.starts_with("**/") {
    format!("**/{pattern}")
} else {
    pattern.to_owned()
};
```

Event filtering (ignoring build artifacts, VCS directories, etc.) should happen before
events enter a bounded channel, not after. Filtering after send wastes channel capacity
and risks stalling the OS watcher thread if the channel fills.

`SyncSender::send` blocks when the channel is full — it does not drop. Use `try_send`
when the intent is to discard events under backpressure.

Duplicate `registerCapability` IDs must be handled explicitly. The LSP spec treats
re-registration as an error; silently overwriting the old registration loses the
previous glob set without notice.

## LSP client

Server-to-client requests (messages with both `method` and `id`) must receive a
JSON-RPC response. Unhandled methods must return error code `-32601` (Method Not Found)
— dropping them silently causes the server to wait indefinitely for an acknowledgement.

Response classification must require either `result` or `error` to be present. A
message with only `id` and no `method`, `result`, or `error` is a protocol error and
must not be coerced into a successful `null` response.
