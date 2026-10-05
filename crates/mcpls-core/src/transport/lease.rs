//! Bounded lifetime ("lease") of stateless `subscriptions/listen` streams.
//!
//! A 2026-07-28 listen stream has no session and the client cannot answer a
//! server `ping`, so a peer that vanished without closing its connection
//! (sleeping laptop, dropped NAT mapping, proxy that kept the upstream open)
//! can only be detected by ending the stream on a timer. The lease ends the
//! HTTP response body abruptly, without the final JSON-RPC result: per the
//! spec a final result means a clean close, whereas an abrupt close is the
//! signal that makes a client listen again. A live client re-listens at once;
//! a vanished one never does and its stream slot is free after one lease.
//!
//! The lease is flagged by `listen()` itself, through a [`ListenLeaseSlot`]
//! that [`attach_listen_lease`] puts into the request extensions, so no
//! request header has to be matched.

use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use futures::task::AtomicWaker;
use tokio::time::{Instant, Sleep};

const NANOS_PER_SEC: u128 = 1_000_000_000;

/// Lifetime window of a listen stream: each stream draws its own length
/// uniformly from `[min, max]` so that streams opened together do not expire
/// together.
///
/// # Examples
///
/// ```
/// use std::time::Duration;
/// use mcpls_core::transport::LeaseWindow;
///
/// assert!(LeaseWindow::new(Duration::ZERO, Duration::from_secs(1)).is_none());
/// assert!(LeaseWindow::new(Duration::from_secs(2), Duration::from_secs(1)).is_none());
/// let window = LeaseWindow::new(Duration::from_secs(1), Duration::from_secs(2)).unwrap();
/// assert_eq!(window.min(), Duration::from_secs(1));
/// assert_eq!(window.max(), Duration::from_secs(2));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseWindow {
    min: Duration,
    max: Duration,
}

impl LeaseWindow {
    /// 15 to 30 minutes.
    pub const DEFAULT: Self = Self {
        min: Duration::from_mins(15),
        max: Duration::from_mins(30),
    };

    /// `None` unless `0 < min <= max` and `max - min` fits in `u64`
    /// nanoseconds (about 584 years), which keeps the jitter draw exact.
    #[must_use]
    pub const fn new(min: Duration, max: Duration) -> Option<Self> {
        if min.is_zero()
            || min.as_nanos() > max.as_nanos()
            || max.as_nanos().saturating_sub(min.as_nanos()) > u64::MAX as u128
        {
            None
        } else {
            Some(Self { min, max })
        }
    }

    /// Shortest lease, never zero.
    #[must_use]
    pub const fn min(self) -> Duration {
        self.min
    }

    /// Longest lease, never below [`Self::min`].
    #[must_use]
    pub const fn max(self) -> Duration {
        self.max
    }

    /// Maps `fraction`, uniform over `u64`, to a duration in `[min, max]`.
    fn at(self, fraction: u64) -> Duration {
        let span = self.max.saturating_sub(self.min).as_nanos();
        // `new` bounds span < 2^64: the product fits u128 and the shift keeps it <= span.
        let offset = u128::from(fraction).saturating_mul(span.saturating_add(1)) >> 64;
        let secs = u64::try_from(offset / NANOS_PER_SEC).unwrap_or(u64::MAX);
        let nanos = u32::try_from(offset % NANOS_PER_SEC).unwrap_or(0);
        self.min.saturating_add(Duration::new(secs, nanos))
    }

    /// A fresh random lease length within the window.
    fn draw(self) -> Duration {
        use std::hash::BuildHasher as _;

        self.at(std::hash::RandomState::new().hash_one(0_u8))
    }
}

/// Bound on the lifetime of stateless `subscriptions/listen` streams over
/// HTTP.
///
/// With [`ListenLease::Renew`] a stream is ended abruptly after a random
/// lease and a client that is still alive listens again; a client that never
/// re-listens stops receiving push updates after one lease but can still
/// read resources. With [`ListenLease::Unbounded`] a stream lives until the
/// client closes it or the OS gives up on the connection.
///
/// # Examples
///
/// ```
/// use mcpls_core::transport::{LeaseWindow, ListenLease};
///
/// assert_eq!(ListenLease::default(), ListenLease::Renew(LeaseWindow::DEFAULT));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenLease {
    /// Never end a listen stream on a timer.
    Unbounded,
    /// End each stream after a lease drawn from the window.
    Renew(LeaseWindow),
}

impl Default for ListenLease {
    fn default() -> Self {
        Self::Renew(LeaseWindow::DEFAULT)
    }
}

/// Per-request flag through which `listen()` starts its lease.
///
/// Created by [`attach_listen_lease`] for every POST; [`LeasedBody`] holds
/// the other end and ends the response once the deadline passes.
#[derive(Debug)]
pub struct ListenLeaseSlot {
    window: LeaseWindow,
    deadline: OnceLock<Instant>,
    waker: AtomicWaker,
}

impl ListenLeaseSlot {
    const fn new(window: LeaseWindow) -> Self {
        Self {
            window,
            deadline: OnceLock::new(),
            waker: AtomicWaker::new(),
        }
    }

    /// Starts the lease now with a freshly drawn length; later calls are
    /// no-ops.
    pub fn start(&self) {
        if self
            .deadline
            .set(super::saturating_deadline(
                Instant::now(),
                self.window.draw(),
            ))
            .is_ok()
        {
            self.waker.wake();
        }
    }
}

/// Response body that ends, without a final JSON-RPC message, once its
/// slot's lease has elapsed.
struct LeasedBody {
    inner: axum::body::Body,
    slot: Arc<ListenLeaseSlot>,
    expiry: Option<Pin<Box<Sleep>>>,
}

impl http_body::Body for LeasedBody {
    type Data = axum::body::Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        use std::future::Future as _;

        let this = self.get_mut();
        if this.expiry.is_none() {
            this.slot.waker.register(cx.waker());
            // Re-checked after registering so a start in between is not missed.
            if let Some(deadline) = this.slot.deadline.get() {
                this.expiry = Some(Box::pin(tokio::time::sleep_until(*deadline)));
            }
        }
        if let Some(expiry) = this.expiry.as_mut()
            && expiry.as_mut().poll(cx).is_ready()
        {
            return Poll::Ready(None);
        }
        Pin::new(&mut this.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

fn is_event_stream(response: &axum::response::Response) -> bool {
    response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"))
}

/// Gives every POST a [`ListenLeaseSlot`] and bounds its SSE response by it.
///
/// A response whose slot `listen()` never starts behaves exactly as without
/// this layer.
pub(super) async fn attach_listen_lease(
    axum::extract::State(lease): axum::extract::State<ListenLease>,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let ListenLease::Renew(window) = lease else {
        return next.run(request).await;
    };
    if request.method() != axum::http::Method::POST {
        return next.run(request).await;
    }
    let slot = Arc::new(ListenLeaseSlot::new(window));
    request.extensions_mut().insert(Arc::clone(&slot));
    let response = next.run(request).await;
    if !is_event_stream(&response) {
        return response;
    }
    let (parts, body) = response.into_parts();
    let body = axum::body::Body::new(LeasedBody {
        inner: body,
        slot,
        expiry: None,
    });
    axum::response::Response::from_parts(parts, body)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn window(min: u64, max: u64) -> LeaseWindow {
        LeaseWindow::new(Duration::from_secs(min), Duration::from_secs(max)).unwrap()
    }

    #[test]
    fn test_window_rejects_zero_and_inverted_bounds() {
        assert!(LeaseWindow::new(Duration::ZERO, Duration::from_secs(1)).is_none());
        assert!(LeaseWindow::new(Duration::from_secs(2), Duration::from_secs(1)).is_none());
        assert!(LeaseWindow::new(Duration::from_secs(1), Duration::from_secs(1)).is_some());
    }

    #[test]
    fn test_window_rejects_a_span_beyond_u64_nanos() {
        let min = Duration::from_secs(1);
        let widest_max = min.saturating_add(Duration::from_nanos(u64::MAX));
        let too_long = widest_max.saturating_add(Duration::from_nanos(1));
        assert!(LeaseWindow::new(min, too_long).is_none());
        let widest = LeaseWindow::new(min, widest_max).unwrap();
        assert_eq!(widest.at(u64::MAX), widest.max());
        assert_eq!(widest.at(0), widest.min());
    }

    #[test]
    fn test_window_at_hits_both_bounds() {
        let w = window(10, 20);
        assert_eq!(w.at(0), Duration::from_secs(10));
        assert_eq!(w.at(u64::MAX), Duration::from_secs(20));
    }

    #[test]
    fn test_window_degenerate_is_constant() {
        let w = window(5, 5);
        assert_eq!(w.at(0), Duration::from_secs(5));
        assert_eq!(w.at(u64::MAX / 2), Duration::from_secs(5));
    }

    #[test]
    fn test_default_window_is_fifteen_to_thirty_minutes() {
        assert_eq!(LeaseWindow::DEFAULT.min(), Duration::from_mins(15));
        assert_eq!(LeaseWindow::DEFAULT.max(), Duration::from_mins(30));
    }

    #[test]
    fn test_draw_stays_within_window_and_varies() {
        let w = LeaseWindow::DEFAULT;
        let draws: Vec<_> = (0..64).map(|_| w.draw()).collect();
        assert!(draws.iter().all(|d| (w.min()..=w.max()).contains(d)));
        assert!(
            draws.iter().any(|d| *d != draws[0]),
            "draws must be jittered"
        );
    }

    proptest! {
        #[test]
        fn prop_at_is_within_window_and_monotonic(
            min in 1_u64..100_000,
            extra in 0_u64..100_000,
            a: u64,
            b: u64,
        ) {
            let w = window(min, min + extra);
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            prop_assert!((w.min()..=w.max()).contains(&w.at(lo)));
            prop_assert!(w.at(lo) <= w.at(hi));
        }
    }

    async fn next_data(body: &mut LeasedBody) -> Option<Vec<u8>> {
        let frame =
            std::future::poll_fn(|cx| http_body::Body::poll_frame(Pin::new(&mut *body), cx))
                .await?
                .unwrap();
        Some(frame.into_data().unwrap().to_vec())
    }

    fn leased(
        window: LeaseWindow,
    ) -> (
        tokio::sync::mpsc::Sender<Result<axum::body::Bytes, std::io::Error>>,
        Arc<ListenLeaseSlot>,
        LeasedBody,
    ) {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let slot = Arc::new(ListenLeaseSlot::new(window));
        let body = LeasedBody {
            inner: axum::body::Body::from_stream(futures::stream::poll_fn(move |cx| {
                rx.poll_recv(cx)
            })),
            slot: Arc::clone(&slot),
            expiry: None,
        };
        (tx, slot, body)
    }

    #[tokio::test(start_paused = true)]
    async fn test_unstarted_lease_never_ends_the_body() {
        let (tx, _slot, mut body) = leased(window(10, 10));
        tx.send(Ok(axum::body::Bytes::from_static(b"a")))
            .await
            .unwrap();
        assert_eq!(next_data(&mut body).await.unwrap(), b"a");
        tokio::time::sleep(Duration::from_secs(3600)).await;
        tx.send(Ok(axum::body::Bytes::from_static(b"b")))
            .await
            .unwrap();
        assert_eq!(next_data(&mut body).await.unwrap(), b"b");
    }

    #[tokio::test(start_paused = true)]
    async fn test_started_lease_ends_a_parked_body_at_its_deadline() {
        let (_tx, slot, mut body) = leased(window(10, 10));
        let begun = Instant::now();
        let next = tokio::spawn(async move { next_data(&mut body).await });
        tokio::time::sleep(Duration::from_secs(1)).await;
        slot.start();
        assert!(next.await.unwrap().is_none());
        assert_eq!(begun.elapsed(), Duration::from_secs(11));
    }

    #[tokio::test(start_paused = true)]
    async fn test_started_lease_ends_a_chatty_body() {
        let (tx, slot, mut body) = leased(window(10, 10));
        slot.start();
        tokio::time::sleep(Duration::from_secs(11)).await;
        tx.send(Ok(axum::body::Bytes::from_static(b"late")))
            .await
            .unwrap();
        assert!(next_data(&mut body).await.is_none());
    }
}
