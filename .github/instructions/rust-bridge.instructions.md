---
applyTo: "crates/mcpls-core/src/bridge/**,crates/mcpls-core/src/lsp/**,crates/mcpls-core/src/lib.rs"
---

## Bridge and LSP layer review checklist

### Document state (`bridge/state.rs`)

- `ensure_open` stats the file on every call and compares `(mtime, size)` against the
  cached `SyncSignature`. On mismatch it must send `didClose` then `didOpen` with a
  bumped version before returning. Verify this resync path is exercised by a test.
- Stat must happen on the same `File` handle as the subsequent read, or be re-done after
  the read, to avoid TOCTOU: an atomic `rename(2)` between stat and `read_to_string` on
  a coarse-mtime filesystem (ext4: 1 s, FAT32: 2 s) leaves the cached signature stale
  permanently.
- Version counter must reset to 1 on overflow, not saturate. `saturating_add(1)` at
  `i32::MAX` sends the same version on every subsequent resync; rust-analyzer silently
  discards non-monotone `didOpen` notifications.

### Diagnostics pipeline (`bridge/translator.rs`, `bridge/notifications.rs`)

- `DiagnosticsMode::Cached` returns an empty `Vec` for both "no errors" and "server
  has not yet analysed this file". AI clients cannot distinguish these. Flag if callers
  treat empty as "clean".
- `merge_diagnostics` dedup key is `(range, severity, code)` when `code` is present,
  falling back to a path-qualifier-stripped message otherwise. Doc comments that say
  `(range, message, code)` are wrong — flag them.
- `pull_diagnostics` has a 30 s timeout. In `Hybrid` mode it runs before the cache read.
  The cache read is a synchronous hashmap lookup; it should not be blocked behind the
  pull. Prefer `tokio::join!` for the two sources.

### Notification pump (`lib.rs`)

- `notification_pump` must hold only a `NotificationCache` lock, never the full
  `Translator` lock. Holding `Mutex<Translator>` across an LSP round-trip in a request
  handler while the pump waits for the same lock causes head-of-line deadlock when the
  notification channel fills.

### File watcher (`lsp/file_watcher.rs`)

- `GlobPattern::String` patterns without a leading `/` or `**/` must be anchored with
  `**/` before passing to `globset`. Bare `*.rs` does not match `/repo/src/lib.rs`.
- `SyncSender::send` blocks when the channel is full. Use `try_send` with a warn log
  when the documented intent is drop-on-full.
- `NEVER_FORWARD_COMPONENTS` filtering should happen before the channel send, not after,
  to avoid burning channel capacity on `target/` churn during `cargo build`.
- `register()` overwrites duplicate registration IDs silently. LSP spec treats
  re-registration as an error; at minimum log a warning.

### LSP client (`lsp/client.rs`, `lsp/transport.rs`)

- Server-to-client requests (`method` + `id`) must receive a JSON-RPC response.
  Unhandled methods should return error code `-32601` (Method Not Found), not be dropped.
- `id`-only messages (no `method`, no `result`, no `error`) are protocol errors and must
  not be treated as successful `null` responses.
