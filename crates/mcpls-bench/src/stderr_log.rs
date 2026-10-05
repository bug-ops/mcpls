//! Size-capped capture of the mcpls stderr stream.
//!
//! The stream is always drained to its end, so a chatty mcpls never blocks on a
//! full pipe, but only the first `cap` bytes reach the log file.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::task::JoinHandle;

use crate::report::StderrLogRecord;

const CHUNK: usize = 8 * 1024;
const MIB: u64 = 1024 * 1024;

/// Default value of `--stderr-log-max-mib`.
pub const DEFAULT_CAP_MIB: u32 = 32;

/// How long the drain may take to reach end-of-stream once mcpls has exited.
pub const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Largest number of bytes of one stderr stream that is written to the log file.
///
/// # Examples
///
/// ```
/// use mcpls_bench::stderr_log::StderrLogCap;
///
/// assert_eq!(StderrLogCap::from_mib(2).bytes(), 2 * 1024 * 1024);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StderrLogCap(u64);

impl StderrLogCap {
    /// A cap of `mib` mebibytes.
    #[must_use]
    pub fn from_mib(mib: u32) -> Self {
        Self(u64::from(mib) * MIB)
    }

    /// A cap of exactly `bytes` bytes.
    #[must_use]
    pub const fn from_bytes(bytes: u64) -> Self {
        Self(bytes)
    }

    /// The cap in bytes.
    #[must_use]
    pub const fn bytes(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Default)]
struct Counters {
    written: AtomicU64,
    dropped: AtomicU64,
}

/// A byte count as `u64`; saturates on a platform where `usize` is wider.
fn widen(count: usize) -> u64 {
    u64::try_from(count).unwrap_or(u64::MAX)
}

fn truncation_marker(cap: StderrLogCap) -> String {
    format!(
        "\n[mcpls-bench: stderr log truncated after {} bytes]\n",
        cap.bytes()
    )
}

async fn capped_copy<R, W>(
    mut reader: R,
    mut writer: W,
    cap: StderrLogCap,
    counters: &Counters,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buffer = [0_u8; CHUNK];
    let mut write_error = None;
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .context("failed to read mcpls stderr")?;
        if read == 0 {
            break;
        }
        let room = cap
            .bytes()
            .saturating_sub(counters.written.load(Ordering::Relaxed));
        let keep = usize::try_from(room).map_or(read, |room| read.min(room));
        if keep > 0 && write_error.is_none() {
            match writer.write_all(&buffer[..keep]).await {
                Ok(()) => {
                    counters.written.fetch_add(widen(keep), Ordering::Relaxed);
                }
                Err(error) => write_error = Some(error),
            }
        }
        let kept = if write_error.is_none() { keep } else { 0 };
        let lost = widen(read - kept);
        if lost > 0 {
            let first_loss = counters.dropped.fetch_add(lost, Ordering::Relaxed) == 0;
            if first_loss && write_error.is_none() {
                write_error = writer
                    .write_all(truncation_marker(cap).as_bytes())
                    .await
                    .err();
            }
        }
    }
    let flushed = writer.flush().await;
    write_error.map_or_else(
        || flushed.context("failed to flush the stderr log"),
        |error| Err(error).context("failed to write the stderr log"),
    )
}

/// A running task that copies a child's stderr into a capped log file.
#[derive(Debug)]
pub struct StderrDrain {
    task: JoinHandle<Result<()>>,
    counters: Arc<Counters>,
    path: PathBuf,
}

impl StderrDrain {
    /// Creates `path` (and its parent directory) and starts copying `stderr` into it.
    ///
    /// # Errors
    ///
    /// Returns an error when the log file cannot be created.
    pub fn start<R>(stderr: R, path: &Path, cap: StderrLogCap) -> Result<Self>
    where
        R: AsyncRead + Send + Unpin + 'static,
    {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let file = std::fs::File::create(path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        let counters = Arc::new(Counters::default());
        let shared = Arc::clone(&counters);
        let task = tokio::spawn(async move {
            capped_copy(stderr, tokio::fs::File::from_std(file), cap, &shared).await
        });
        Ok(Self {
            task,
            counters,
            path: path.to_path_buf(),
        })
    }

    /// Waits up to `grace` for end-of-stream, then aborts the copy and reports what was captured.
    pub async fn finish(mut self, grace: Duration) -> StderrLogRecord {
        let (complete, error) = match tokio::time::timeout(grace, &mut self.task).await {
            Ok(Ok(Ok(()))) => (true, None),
            Ok(Ok(Err(error))) => (true, Some(format!("{error:#}"))),
            Ok(Err(join)) => (false, Some(join.to_string())),
            Err(_) => {
                self.task.abort();
                (false, None)
            }
        };
        StderrLogRecord {
            path: self.path,
            written_bytes: self.counters.written.load(Ordering::Relaxed),
            dropped_bytes: self.counters.dropped.load(Ordering::Relaxed),
            drain_complete: complete,
            error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn copy(input: &[u8], cap: u64) -> (Vec<u8>, Counters) {
        let counters = Counters::default();
        let mut out = Vec::new();
        capped_copy(input, &mut out, StderrLogCap::from_bytes(cap), &counters)
            .await
            .unwrap();
        (out, counters)
    }

    #[tokio::test]
    async fn output_below_the_cap_is_kept_whole() {
        let (out, counters) = copy(b"hello", 100).await;
        assert_eq!(out, b"hello");
        assert_eq!(counters.written.load(Ordering::Relaxed), 5);
        assert_eq!(counters.dropped.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn output_beyond_the_cap_is_dropped_and_marked_once() {
        let input = vec![b'x'; 3 * CHUNK + 17];
        let (out, counters) = copy(&input, 10).await;
        assert!(out.starts_with(&[b'x'; 10]));
        let text = String::from_utf8(out[10..].to_vec()).unwrap();
        assert_eq!(text, truncation_marker(StderrLogCap::from_bytes(10)));
        assert_eq!(counters.written.load(Ordering::Relaxed), 10);
        assert_eq!(
            counters.dropped.load(Ordering::Relaxed),
            widen(input.len()) - 10
        );
    }

    #[tokio::test]
    async fn a_zero_cap_keeps_nothing_but_still_drains() {
        let (out, counters) = copy(b"abc", 0).await;
        assert!(String::from_utf8(out).unwrap().contains("truncated"));
        assert_eq!(counters.dropped.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn a_stalled_stream_is_abandoned_after_the_grace() {
        let dir = tempfile::tempdir().unwrap();
        let (_held_open, reader) = tokio::io::duplex(64);
        let path = dir.path().join("logs/run-0.log");
        let drain = StderrDrain::start(reader, &path, StderrLogCap::from_bytes(100)).unwrap();
        let record = drain.finish(Duration::from_millis(50)).await;
        assert!(!record.drain_complete);
        assert_eq!(record.path, path);
    }

    #[tokio::test]
    async fn a_finished_stream_reports_its_counts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run-1.log");
        let drain =
            StderrDrain::start(&b"warn: x\n"[..], &path, StderrLogCap::from_bytes(4)).unwrap();
        let record = drain.finish(Duration::from_secs(5)).await;
        assert!(record.drain_complete);
        assert_eq!((record.written_bytes, record.dropped_bytes), (4, 4));
        assert!(std::fs::read_to_string(&path).unwrap().starts_with("warn"));
    }
}
