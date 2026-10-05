//! Per-connection I/O wrapper for the HTTP transport.
//!
//! [`ConnectionIo`] adds two bounds hyper does not provide:
//!
//! - a write-stall deadline: a write the peer does not drain at a minimum
//!   rate fails with [`io::ErrorKind::TimedOut`], so a peer that stops
//!   reading, or reads a trickle, frees its connection permit. A write that
//!   goes pending opens a stall window of [`WriteStallTimeout`]; the window
//!   closes when a flush completes or the peer has taken at least
//!   [`MIN_WRITE_RATE_BYTES_PER_SEC`] times the window in bytes, and the
//!   connection is cut when it expires first;
//! - a lingering close: after the FIN, incoming bytes are discarded until the
//!   peer closes, so a response sent while the request body is still unread
//!   (an early `403` or `413`) is not turned into a reset that eats the status.

use std::future::Future as _;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::{Instant, Sleep};
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};

use super::{HeaderReadTimeout, WriteStallTimeout};

/// Longest the lingering close waits for the next byte or the EOF.
const LINGER_READ_IDLE: Duration = Duration::from_secs(2);

/// Upper bound of the whole lingering close, whatever `header_read_timeout` is.
const MAX_LINGER: Duration = Duration::from_secs(30);

/// Most bytes the lingering close discards before it gives up on the peer.
const LINGER_MAX_BYTES: usize = 1 << 20;

/// Reads per poll while lingering, so a flooding peer cannot starve the runtime.
const LINGER_READS_PER_POLL: usize = 16;

const LINGER_READ_BUFFER: usize = 4096;

/// Slowest peer drain rate, in bytes per second, that keeps a stall window
/// from expiring.
const MIN_WRITE_RATE_BYTES_PER_SEC: u32 = 2048;

/// Bounds of the bytes a peer must take within one stall window.
const MIN_PROGRESS_FLOOR: usize = 4 * 1024;
const MIN_PROGRESS_CEILING: usize = 1 << 20;

/// Bytes the peer must take within a stall window of `window`.
fn min_progress(window: Duration) -> usize {
    let bytes = window
        .as_millis()
        .saturating_mul(u128::from(MIN_WRITE_RATE_BYTES_PER_SEC))
        / 1000;
    usize::try_from(bytes)
        .unwrap_or(usize::MAX)
        .clamp(MIN_PROGRESS_FLOOR, MIN_PROGRESS_CEILING)
}

/// `now + after`, or a year ahead (effectively never) when the sum overflows.
pub(super) fn deadline_after(now: Instant, after: Duration) -> Instant {
    now.checked_add(after)
        .or_else(|| now.checked_add(Duration::from_hours(24 * 365)))
        .unwrap_or(now)
}

/// Bytes a completed write accepted; `0` for a pending or failed one.
const fn bytes_written(poll: &Poll<io::Result<usize>>) -> usize {
    match poll {
        Poll::Ready(Ok(written)) => *written,
        Poll::Ready(Err(_)) | Poll::Pending => 0,
    }
}

/// An open stall window: the bytes the peer has taken since it opened.
#[derive(Debug, Clone, Copy)]
struct StallWindow {
    progress: usize,
}

#[derive(Debug, Clone, Copy)]
enum Phase {
    Serving { stall: Option<StallWindow> },
    Lingering { total_deadline: Instant },
    Done,
}

/// Wraps a connection's stream with the write-stall deadline and the
/// lingering close.
///
/// One reusable timer serves both: it is the stall deadline while
/// [`Phase::Serving`] and the read-idle deadline while [`Phase::Lingering`].
/// The linger is skipped, or ended, once the server is shutting down.
#[derive(Debug)]
pub(super) struct ConnectionIo<T> {
    inner: T,
    timer: Pin<Box<Sleep>>,
    shutdown: Pin<Box<WaitForCancellationFutureOwned>>,
    write_stall: Duration,
    min_progress: usize,
    linger_total: Duration,
    discarded: usize,
    phase: Phase,
}

impl<T> ConnectionIo<T> {
    /// Wraps `inner`. The lingering close lasts at most
    /// `min(header_read_timeout, MAX_LINGER)` and `LINGER_MAX_BYTES`, and not
    /// at all once `shutdown` is cancelled.
    ///
    /// Must be called inside a Tokio runtime.
    pub(super) fn new(
        inner: T,
        write_stall: WriteStallTimeout,
        header_read_timeout: HeaderReadTimeout,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            inner,
            timer: Box::pin(tokio::time::sleep(write_stall.get())),
            shutdown: Box::pin(shutdown.cancelled_owned()),
            write_stall: write_stall.get(),
            min_progress: min_progress(write_stall.get()),
            linger_total: header_read_timeout.get().min(MAX_LINGER),
            discarded: 0,
            phase: Phase::Serving { stall: None },
        }
    }

    /// Applies the stall window to the outcome of a write-side poll.
    ///
    /// `flushed` is true for a completed flush: hyper flushes only once its
    /// write buffer is empty, so that is the one proof the peer drained
    /// everything it was offered. A write that completes only adds to the
    /// window's progress; hyper may keep writing into a peer that took a few
    /// bytes per window, and the window then still expires.
    fn watch_write<R>(
        &mut self,
        cx: &mut Context<'_>,
        poll: Poll<io::Result<R>>,
        flushed: bool,
        written: usize,
    ) -> Poll<io::Result<R>> {
        let Phase::Serving { stall } = &mut self.phase else {
            return poll;
        };
        if matches!(poll, Poll::Ready(Err(_))) || (flushed && poll.is_ready()) {
            *stall = None;
            return poll;
        }
        let window = match (stall.as_mut(), &poll) {
            (Some(window), _) => window,
            (None, Poll::Pending) => {
                self.timer
                    .as_mut()
                    .reset(deadline_after(Instant::now(), self.write_stall));
                stall.insert(StallWindow { progress: 0 })
            }
            (None, Poll::Ready(_)) => return poll,
        };
        window.progress = window.progress.saturating_add(written);
        if window.progress >= self.min_progress {
            *stall = None;
            return poll;
        }
        if self.timer.as_mut().poll(cx).is_ready() {
            // A peer that stopped reading gets no linger: dropping the stream closes it.
            self.phase = Phase::Done;
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "write to the peer made no progress",
            )));
        }
        poll
    }

    /// Enters the linger unless the server is already shutting down.
    fn begin_linger(&mut self, cx: &mut Context<'_>) {
        if self.shutdown.as_mut().poll(cx).is_ready() {
            self.phase = Phase::Done;
            return;
        }
        let now = Instant::now();
        let total_deadline = deadline_after(now, self.linger_total);
        self.timer
            .as_mut()
            .reset(deadline_after(now, LINGER_READ_IDLE).min(total_deadline));
        self.discarded = 0;
        self.phase = Phase::Lingering { total_deadline };
    }

    const fn finish_linger(&mut self) -> Poll<io::Result<()>> {
        self.phase = Phase::Done;
        Poll::Ready(Ok(()))
    }
}

impl<T: AsyncRead + Unpin> ConnectionIo<T> {
    fn poll_linger(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let Phase::Lingering { total_deadline } = self.phase else {
            return Poll::Ready(Ok(()));
        };
        if self.shutdown.as_mut().poll(cx).is_ready() {
            return self.finish_linger();
        }
        let mut scratch = [0u8; LINGER_READ_BUFFER];
        let mut progressed = false;
        let mut drained = false;
        for _ in 0..LINGER_READS_PER_POLL {
            let mut buf = ReadBuf::new(&mut scratch);
            match Pin::new(&mut self.inner).poll_read(cx, &mut buf) {
                Poll::Ready(Ok(())) if buf.filled().is_empty() => return self.finish_linger(),
                Poll::Ready(Ok(())) => {
                    progressed = true;
                    self.discarded = self.discarded.saturating_add(buf.filled().len());
                    if self.discarded >= LINGER_MAX_BYTES {
                        return self.finish_linger();
                    }
                }
                Poll::Ready(Err(_)) => return self.finish_linger(),
                Poll::Pending => {
                    drained = true;
                    break;
                }
            }
        }
        if progressed {
            self.timer
                .as_mut()
                .reset(deadline_after(Instant::now(), LINGER_READ_IDLE).min(total_deadline));
        }
        if self.timer.as_mut().poll(cx).is_ready() {
            return self.finish_linger();
        }
        if !drained {
            // Reads were still ready after the per-poll budget: yield, then continue.
            cx.waker().wake_by_ref();
        }
        Poll::Pending
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for ConnectionIo<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> AsyncWrite for ConnectionIo<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.inner).poll_write(cx, buf);
        let written = bytes_written(&poll);
        this.watch_write(cx, poll, false, written)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.inner).poll_write_vectored(cx, bufs);
        let written = bytes_written(&poll);
        this.watch_write(cx, poll, false, written)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.inner).poll_flush(cx);
        this.watch_write(cx, poll, true, 0)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if matches!(this.phase, Phase::Serving { .. }) {
            let poll = Pin::new(&mut this.inner).poll_shutdown(cx);
            match this.watch_write(cx, poll, true, 0) {
                Poll::Ready(Ok(())) => this.begin_linger(cx),
                other => return other,
            }
        }
        this.poll_linger(cx)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::assert_matches;
    use std::time::Duration;

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::time::Instant;

    use super::*;

    const STALL: Duration = Duration::from_secs(10);

    fn stall() -> WriteStallTimeout {
        WriteStallTimeout::new(STALL).unwrap()
    }

    fn header_timeout(timeout: Duration) -> HeaderReadTimeout {
        HeaderReadTimeout::new(timeout).unwrap()
    }

    fn wrap<T>(inner: T) -> ConnectionIo<T> {
        ConnectionIo::new(
            inner,
            stall(),
            HeaderReadTimeout::DEFAULT,
            CancellationToken::new(),
        )
    }

    /// Never accepts a write or a shutdown and never has data to read.
    struct Stuck;

    impl AsyncRead for Stuck {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    impl AsyncWrite for Stuck {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Pending
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Pending
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    #[tokio::test(start_paused = true)]
    async fn test_stalled_write_times_out_after_the_deadline() {
        let (near, _far) = tokio::io::duplex(16);
        let mut io = wrap(near);
        let start = Instant::now();

        let err = io.write_all(&[0; 64]).await.unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert_eq!(start.elapsed(), STALL);
    }

    #[tokio::test(start_paused = true)]
    async fn test_a_timed_out_connection_does_not_linger_on_shutdown() {
        let (near, _far) = tokio::io::duplex(16);
        let mut io = wrap(near);
        io.write_all(&[0; 64]).await.unwrap_err();
        let start = Instant::now();

        io.shutdown().await.unwrap();

        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn test_stalled_flush_times_out_after_the_deadline() {
        let mut io = wrap(Stuck);
        let start = Instant::now();

        let err = io.flush().await.unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert_eq!(start.elapsed(), STALL);
    }

    #[tokio::test(start_paused = true)]
    async fn test_stalled_vectored_write_times_out_after_the_deadline() {
        let mut io = wrap(Stuck);
        let start = Instant::now();
        let bufs = [io::IoSlice::new(b"head"), io::IoSlice::new(b"body")];

        let err = io.write_vectored(&bufs).await.unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert_eq!(start.elapsed(), STALL);
    }

    #[tokio::test(start_paused = true)]
    async fn test_stalled_shutdown_times_out_after_the_deadline() {
        let mut io = wrap(Stuck);
        let start = Instant::now();

        let err = io.shutdown().await.unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert_eq!(start.elapsed(), STALL);
    }

    #[tokio::test(start_paused = true)]
    async fn test_a_peer_trickling_below_the_minimum_rate_is_cut_at_the_first_deadline() {
        let (near, mut far) = tokio::io::duplex(8);
        let mut io = wrap(near);
        let start = Instant::now();
        let writer = tokio::spawn(async move { io.write_all(&vec![0; 100_000]).await });
        let reader = tokio::spawn(async move {
            let mut chunk = [0u8; 8];
            loop {
                tokio::time::sleep(STALL / 4).await;
                if far.read_exact(&mut chunk).await.is_err() {
                    break;
                }
            }
        });

        let err = writer.await.unwrap().unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert_eq!(start.elapsed(), STALL);
        reader.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn test_a_peer_draining_at_the_minimum_rate_survives_many_windows() {
        let (near, mut far) = tokio::io::duplex(32 * 1024);
        let mut io = wrap(near);
        let start = Instant::now();
        let total = 400_000usize;
        let writer = tokio::spawn(async move { io.write_all(&vec![0; total]).await });
        let reader = tokio::spawn(async move {
            let mut taken = 0;
            let mut chunk = vec![0u8; 32 * 1024];
            while taken < total {
                tokio::time::sleep(STALL / 2).await;
                taken += far.read(&mut chunk).await.unwrap();
            }
        });

        writer.await.unwrap().unwrap();
        reader.await.unwrap();

        assert!(start.elapsed() > STALL * 3, "{:?}", start.elapsed());
    }

    #[tokio::test(start_paused = true)]
    async fn test_writes_the_peer_accepts_at_once_never_open_a_window() {
        let (near, _far) = tokio::io::duplex(1024);
        let mut io = wrap(near);
        for _ in 0..8 {
            io.write_all(&[1; 16]).await.unwrap();
            tokio::time::sleep(STALL * 2).await;
        }
        assert_matches!(io.phase, Phase::Serving { stall: None });
    }

    #[tokio::test(start_paused = true)]
    async fn test_a_completed_flush_closes_the_window_and_the_next_stall_starts_fresh() {
        let (near, mut far) = tokio::io::duplex(64);
        let mut io = wrap(near);
        let start = Instant::now();
        io.write_all(&[0; 64]).await.unwrap();
        let writer = tokio::spawn(async move {
            io.write_all(&[0; 64]).await.unwrap();
            io.flush().await.unwrap();
            io.write_all(&[0; 128]).await
        });

        tokio::time::sleep(STALL / 2).await;
        let mut drained = [0u8; 64];
        far.read_exact(&mut drained).await.unwrap();
        tokio::time::sleep(STALL * 3 / 4).await;
        assert!(
            !writer.is_finished(),
            "a stall that began after a flush must not fire at the first deadline"
        );

        let err = writer.await.unwrap().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert_eq!(start.elapsed(), STALL / 2 + STALL);
    }

    #[test]
    fn test_min_progress_scales_with_the_window_and_is_clamped() {
        assert_eq!(min_progress(Duration::from_secs(30)), 61_440);
        assert_eq!(min_progress(Duration::from_millis(1)), MIN_PROGRESS_FLOOR);
        assert_eq!(
            min_progress(Duration::from_secs(10_000)),
            MIN_PROGRESS_CEILING
        );
        assert_eq!(min_progress(Duration::MAX), MIN_PROGRESS_CEILING);
    }

    #[tokio::test(start_paused = true)]
    async fn test_shutdown_lingers_until_the_peer_closes() {
        let (near, mut far) = tokio::io::duplex(1024);
        let mut io = wrap(near);
        let start = Instant::now();
        far.write_all(&[7; 100]).await.unwrap();
        let closer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            drop(far);
        });

        io.shutdown().await.unwrap();

        assert_eq!(start.elapsed(), Duration::from_secs(1));
        closer.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn test_lingering_ends_after_the_read_idle_and_never_reports_a_stall() {
        let (near, _far) = tokio::io::duplex(1024);
        let mut io = ConnectionIo::new(
            near,
            WriteStallTimeout::new(Duration::from_secs(1)).unwrap(),
            HeaderReadTimeout::DEFAULT,
            CancellationToken::new(),
        );
        let start = Instant::now();

        io.shutdown().await.unwrap();

        assert_eq!(start.elapsed(), LINGER_READ_IDLE);
    }

    #[tokio::test(start_paused = true)]
    async fn test_shutdown_after_done_is_immediate() {
        let (near, far) = tokio::io::duplex(1024);
        let mut io = wrap(near);
        drop(far);
        io.shutdown().await.unwrap();
        let start = Instant::now();

        io.shutdown().await.unwrap();

        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn test_lingering_ends_at_the_total_cap_against_a_trickling_peer() {
        let (near, mut far) = tokio::io::duplex(64);
        let mut io = ConnectionIo::new(
            near,
            stall(),
            header_timeout(Duration::from_secs(5)),
            CancellationToken::new(),
        );
        let trickle = tokio::spawn(async move {
            while far.write_all(&[1]).await.is_ok() {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        let start = Instant::now();

        io.shutdown().await.unwrap();

        assert_eq!(start.elapsed(), Duration::from_secs(5));
        trickle.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn test_lingering_stops_after_the_byte_cap_against_a_flooding_peer() {
        let (near, mut far) = tokio::io::duplex(64);
        let mut io = wrap(near);
        let flood = tokio::spawn(async move { while far.write_all(&[1; 64]).await.is_ok() {} });
        let start = Instant::now();

        io.shutdown().await.unwrap();

        assert!(io.discarded >= LINGER_MAX_BYTES);
        assert_eq!(start.elapsed(), Duration::ZERO);
        flood.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn test_cancelling_the_server_ends_a_running_linger() {
        let (near, _far) = tokio::io::duplex(64);
        let token = CancellationToken::new();
        let mut io = ConnectionIo::new(near, stall(), HeaderReadTimeout::DEFAULT, token.clone());
        let canceller = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            token.cancel();
        });
        let start = Instant::now();

        io.shutdown().await.unwrap();

        assert_eq!(start.elapsed(), Duration::from_millis(500));
        canceller.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn test_no_linger_starts_once_the_server_is_shutting_down() {
        let (near, _far) = tokio::io::duplex(64);
        let token = CancellationToken::new();
        token.cancel();
        let mut io = ConnectionIo::new(near, stall(), HeaderReadTimeout::DEFAULT, token);
        let start = Instant::now();

        io.shutdown().await.unwrap();

        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[test]
    fn test_linger_total_is_capped_by_max_linger() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        let new = |timeout| {
            ConnectionIo::new(
                Stuck,
                stall(),
                header_timeout(timeout),
                CancellationToken::new(),
            )
        };
        let capped = new(Duration::from_hours(1));
        let short = new(Duration::from_secs(5));

        assert_eq!(capped.linger_total, MAX_LINGER);
        assert_eq!(short.linger_total, Duration::from_secs(5));
    }
}
