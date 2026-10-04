//! Bounded capture of an LSP server's stderr during startup.
//!
//! The drain task owns the child's stderr read end for the whole life of the
//! process: dropping it early would hand the server `EPIPE`/`SIGPIPE` on its
//! next diagnostic write. Only the first [`HEAD_BYTES`] and last
//! [`TAIL_BYTES`] bytes are kept.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex as StdMutex};

use tokio::io::AsyncReadExt as _;
use tokio::process::ChildStderr;
use tokio::sync::watch;
use tokio::time::{Duration, sleep, timeout};
use tracing::{debug, warn};

use crate::bridge::lock_std;
use crate::error::StderrExcerpt;

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
#[derive(Debug, Default)]
struct Ring {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    elided: bool,
}

impl Ring {
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

    fn excerpt(&self, secrets: &[&str]) -> Option<StderrExcerpt> {
        let tail = self.tail.iter().copied().collect::<Vec<u8>>();
        if self.elided {
            StderrExcerpt::elided(&self.head, &tail, secrets)
        } else {
            StderrExcerpt::complete(&[self.head.as_slice(), &tail].concat(), secrets)
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
    eof: watch::Receiver<bool>,
}

impl StderrCapture {
    /// Starts draining `stderr` on a detached task.
    pub(super) fn start(stderr: ChildStderr) -> Self {
        let ring = Arc::new(StdMutex::new(Ring::default()));
        let (eof_tx, eof) = watch::channel(false);
        tokio::spawn(drain(stderr, Arc::clone(&ring), eof_tx));
        Self { ring, eof }
    }

    /// Snapshot of what the server has written so far.
    ///
    /// When `child_exited`, waits briefly for end-of-file first so output
    /// still in flight is included. `secrets` are redacted from the excerpt.
    pub(super) async fn finish(
        &self,
        child_exited: bool,
        secrets: &[&str],
    ) -> Option<StderrExcerpt> {
        if child_exited {
            let mut eof = self.eof.clone();
            if timeout(EOF_GRACE, eof.wait_for(|done| *done))
                .await
                .is_err()
            {
                debug!("stderr still open after the server exited; using a partial snapshot");
            }
        }
        lock_std(&self.ring).excerpt(secrets)
    }
}

async fn drain(mut stderr: ChildStderr, ring: Arc<StdMutex<Ring>>, eof: watch::Sender<bool>) {
    let mut buffer = [0u8; READ_CHUNK_BYTES];
    let mut consecutive_errors = 0_u32;
    loop {
        match stderr.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => {
                consecutive_errors = 0;
                lock_std(&ring).push(&buffer[..read]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => {
                consecutive_errors += 1;
                if consecutive_errors >= MAX_CONSECUTIVE_READ_ERRORS {
                    warn!("giving up on LSP server stderr after repeated read errors: {e}");
                    break;
                }
                debug!("LSP server stderr read failed, retrying: {e}");
                sleep(READ_RETRY_DELAY).await;
            }
        }
    }
    eof.send_replace(true);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_ring_keeps_short_output_complete() {
        let mut ring = Ring::default();
        ring.push(b"hello ");
        ring.push(b"world");

        let excerpt = ring.excerpt(&[]).unwrap();

        assert_eq!(excerpt.head(), "hello world");
        assert!(!excerpt.is_elided());
    }

    #[test]
    fn test_ring_bounds_memory_and_keeps_head_and_tail() {
        let mut ring = Ring::default();
        ring.push(b"HEAD");
        ring.push(&vec![b'x'; HEAD_BYTES + TAIL_BYTES + 500]);
        ring.push(b"TAIL");

        assert_eq!(ring.head.len(), HEAD_BYTES);
        assert_eq!(ring.tail.len(), TAIL_BYTES);
        let excerpt = ring.excerpt(&[]).unwrap();
        assert!(excerpt.is_elided());
        assert!(excerpt.head().starts_with("HEAD"));
        assert!(excerpt.tail().unwrap().ends_with("TAIL"));
    }

    #[test]
    fn test_ring_exactly_filling_both_windows_is_not_elided() {
        let mut ring = Ring::default();
        ring.push(&vec![b'a'; HEAD_BYTES + TAIL_BYTES]);

        assert!(!ring.elided);
    }

    #[test]
    fn test_ring_empty_has_no_excerpt() {
        assert_eq!(Ring::default().excerpt(&[]), None);
    }
}
