//! Bounded capture of an LSP server's stderr during startup.
//!
//! The drain task owns the child's stderr read end for the whole life of the
//! process: dropping it early would hand the server `EPIPE`/`SIGPIPE` on its
//! next diagnostic write. Only the first [`HEAD_BYTES`] and last
//! [`TAIL_BYTES`] bytes are kept.
//!
//! On Unix the lifeline anchor additionally holds a copy of the read end, and
//! the watchdog holds copies of the server's stdin and stdout pipe ends: if
//! mcpls stops draining while it is alive (this drain gave up after repeated
//! read errors, or an embedder's runtime shut down), the server blocks on its
//! next write instead of dying from `EPIPE`, until the lifeline sweep kills it.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex as StdMutex};

use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::time::{Duration, sleep, timeout};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::error::StderrExcerpt;
use crate::redaction::Redactions;
use crate::util::lock_std;

/// Leading bytes of stderr kept verbatim.
const HEAD_BYTES: usize = 1024;

/// Trailing bytes of stderr kept once the output outgrows the head.
const TAIL_BYTES: usize = 3072;

/// How long [`StderrCapture::finish`] waits for end-of-file after the child
/// exited; a grandchild may still hold the pipe open.
const EOF_GRACE: Duration = Duration::from_millis(100);

/// Pause before retrying a failed read, so a broken pipe cannot spin.
const READ_RETRY_DELAY: Duration = Duration::from_secs(1);

/// Consecutive read failures after which the drain gives up.
const MAX_CONSECUTIVE_READ_ERRORS: u32 = 5;

const READ_CHUNK_BYTES: usize = 4096;

/// First [`HEAD_BYTES`] plus last [`TAIL_BYTES`] bytes of a stream.
#[derive(Debug)]
struct Ring {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    elided: bool,
}

impl Ring {
    fn new() -> Self {
        Self {
            head: Vec::with_capacity(HEAD_BYTES),
            tail: VecDeque::with_capacity(TAIL_BYTES),
            elided: false,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        let room = HEAD_BYTES.saturating_sub(self.head.len()).min(chunk.len());
        let (head_part, tail_part) = chunk.split_at(room);
        self.head.extend_from_slice(head_part);
        self.tail.extend(tail_part);
        if let Some(overflow) = self.tail.len().checked_sub(TAIL_BYTES)
            && overflow > 0
        {
            self.tail.drain(..overflow);
            self.elided = true;
        }
    }

    fn excerpt(&self, redactions: &Redactions) -> Option<StderrExcerpt> {
        let tail = self.tail.iter().copied().collect::<Vec<u8>>();
        if self.elided {
            StderrExcerpt::elided(&self.head, &tail, redactions)
        } else {
            StderrExcerpt::complete(&[self.head.as_slice(), &tail].concat(), redactions)
        }
    }
}

/// Handle to a running stderr drain.
///
/// Dropping it detaches the drain, which keeps reading until the child closes
/// its stderr.
#[derive(Debug)]
pub(super) struct StderrCapture {
    ring: Arc<StdMutex<Ring>>,
    eof: CancellationToken,
}

/// Whether [`StderrCapture::finish`] waits for end-of-file before snapshotting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EofWait {
    /// Wait up to `EOF_GRACE`: the server is gone or about to exit.
    Grace,
    /// Snapshot immediately: the server is still running.
    Skip,
}

impl StderrCapture {
    /// Starts draining `stderr` on a detached task.
    pub(super) fn start<R: AsyncRead + Unpin + Send + 'static>(stderr: R) -> Self {
        let ring = Arc::new(StdMutex::new(Ring::new()));
        let eof = CancellationToken::new();
        tokio::spawn(drain(stderr, Arc::clone(&ring), eof.clone()));
        Self { ring, eof }
    }

    /// Snapshot of what the server has written so far.
    ///
    /// With [`EofWait::Grace`], waits briefly for end-of-file first so output
    /// still in flight is included. Secrets in `redactions` are hidden from
    /// the excerpt.
    pub(super) async fn finish(
        &self,
        eof_wait: EofWait,
        redactions: &Redactions,
    ) -> Option<StderrExcerpt> {
        if eof_wait == EofWait::Grace && timeout(EOF_GRACE, self.eof.cancelled()).await.is_err() {
            debug!("stderr still open after the server exited; using a partial snapshot");
        }
        lock_std(&self.ring).excerpt(redactions)
    }
}

async fn drain<R: AsyncRead + Unpin>(
    mut stderr: R,
    ring: Arc<StdMutex<Ring>>,
    eof: CancellationToken,
) {
    let _signal_eof = eof.drop_guard();
    let mut buffer = [0u8; READ_CHUNK_BYTES];
    let mut consecutive_errors = 0_u32;
    loop {
        match stderr.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => {
                consecutive_errors = 0;
                lock_std(&ring).push(buffer.get(..read).unwrap_or_default());
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => {
                consecutive_errors = consecutive_errors.saturating_add(1);
                if consecutive_errors >= MAX_CONSECUTIVE_READ_ERRORS {
                    warn!("giving up on LSP server stderr after repeated read errors: {e}");
                    break;
                }
                debug!("LSP server stderr read failed, retrying: {e}");
                sleep(READ_RETRY_DELAY).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ring_keeps_short_output_complete() {
        let mut ring = Ring::new();
        ring.push(b"hello ");
        ring.push(b"world");

        let excerpt = ring.excerpt(&Redactions::default()).unwrap();

        assert_eq!(excerpt.head(), "hello world");
        assert!(!excerpt.is_elided());
    }

    #[test]
    fn test_ring_bounds_memory_and_keeps_head_and_tail() {
        let mut ring = Ring::new();
        ring.push(b"HEAD");
        ring.push(&vec![b'x'; HEAD_BYTES + TAIL_BYTES + 500]);
        ring.push(b"TAIL");

        assert_eq!(ring.head.len(), HEAD_BYTES);
        assert_eq!(ring.tail.len(), TAIL_BYTES);
        let excerpt = ring.excerpt(&Redactions::default()).unwrap();
        assert!(excerpt.is_elided());
        assert!(excerpt.head().starts_with("HEAD"));
        assert!(excerpt.tail().unwrap().ends_with("TAIL"));
    }

    #[test]
    fn test_ring_exactly_filling_both_windows_is_not_elided() {
        let mut ring = Ring::new();
        ring.push(&vec![b'a'; HEAD_BYTES + TAIL_BYTES]);

        assert!(!ring.elided);
    }

    /// A drain that dies without reaching end-of-file still releases `finish`,
    /// which would otherwise wait out the whole grace period.
    #[tokio::test]
    async fn test_a_dead_drain_releases_the_eof_signal() {
        let (_kept_open, reader) = tokio::io::duplex(8);
        let eof = CancellationToken::new();
        let task = tokio::spawn(drain(
            reader,
            Arc::new(StdMutex::new(Ring::new())),
            eof.clone(),
        ));
        tokio::task::yield_now().await;
        task.abort();

        timeout(std::time::Duration::from_secs(5), eof.cancelled())
            .await
            .expect("a dead drain left the eof signal pending");
    }

    /// Reader replaying scripted results, then reporting end-of-file.
    struct ScriptedReader {
        script: VecDeque<std::io::Result<Vec<u8>>>,
    }

    impl AsyncRead for ScriptedReader {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            match self.script.pop_front() {
                Some(Ok(bytes)) => {
                    buf.put_slice(&bytes);
                    std::task::Poll::Ready(Ok(()))
                }
                Some(Err(e)) => std::task::Poll::Ready(Err(e)),
                None => std::task::Poll::Ready(Ok(())),
            }
        }
    }

    fn failure() -> std::io::Result<Vec<u8>> {
        Err(std::io::Error::other("pipe broke"))
    }

    async fn run_drain(
        script: Vec<std::io::Result<Vec<u8>>>,
    ) -> (Arc<StdMutex<Ring>>, usize, bool) {
        let ring = Arc::new(StdMutex::new(Ring::new()));
        let eof = CancellationToken::new();
        let mut reader = ScriptedReader {
            script: script.into(),
        };
        drain(&mut reader, Arc::clone(&ring), eof.clone()).await;
        (ring, reader.script.len(), eof.is_cancelled())
    }

    #[tokio::test(start_paused = true)]
    async fn test_drain_gives_up_after_consecutive_read_errors() {
        let mut script: Vec<_> = (0..MAX_CONSECUTIVE_READ_ERRORS)
            .map(|_| failure())
            .collect();
        script.push(Ok(b"never read".to_vec()));

        let (ring, unread, eof) = run_drain(script).await;

        assert_eq!(unread, 1, "the drain must stop at the fifth error");
        assert!(eof, "end-of-file must still be signalled");
        assert!(ring.lock().unwrap().head.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn test_drain_resets_error_counter_after_a_successful_read() {
        let failures = MAX_CONSECUTIVE_READ_ERRORS - 1;
        let mut script: Vec<_> = (0..failures).map(|_| failure()).collect();
        script.push(Ok(b"first ".to_vec()));
        script.extend((0..failures).map(|_| failure()));
        script.push(Ok(b"second".to_vec()));

        let (ring, unread, eof) = run_drain(script).await;

        assert_eq!(unread, 0, "the drain must read to end-of-file");
        assert!(eof);
        let excerpt = ring
            .lock()
            .unwrap()
            .excerpt(&Redactions::default())
            .unwrap();
        assert_eq!(excerpt.head(), "first second");
    }

    #[tokio::test(start_paused = true)]
    async fn test_drain_retries_interrupted_reads_immediately() {
        let interrupted = || Err(std::io::Error::from(std::io::ErrorKind::Interrupted));
        let script = (0..MAX_CONSECUTIVE_READ_ERRORS + 2)
            .map(|_| interrupted())
            .chain([Ok(b"after".to_vec())])
            .collect();

        let (ring, unread, _eof) = run_drain(script).await;

        assert_eq!(unread, 0);
        assert!(ring.lock().unwrap().head.starts_with(b"after"));
    }

    #[test]
    fn test_ring_empty_has_no_excerpt() {
        assert_eq!(Ring::new().excerpt(&Redactions::default()), None);
    }
}
