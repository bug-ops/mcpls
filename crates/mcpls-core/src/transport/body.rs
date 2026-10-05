//! Request-body wrappers for the HTTP transport.

use super::config::HeaderReadTimeout;

/// Shortest total time a request body may take, whatever the configured
/// `header_read_timeout` is.
const MIN_BODY_DEADLINE: std::time::Duration = std::time::Duration::from_mins(2);

/// Total time a request body may take: four idle windows, at least
/// [`MIN_BODY_DEADLINE`].
fn body_deadline(header_read_timeout: HeaderReadTimeout) -> std::time::Duration {
    MIN_BODY_DEADLINE.max(header_read_timeout.get().saturating_mul(4))
}

/// Request body that fails once no frame has arrived for `timeout`, or the
/// whole body has taken longer than its total deadline, flagging `expired` so
/// [`enforce_body_inactivity`] can answer `408`.
struct InactivityBody {
    inner: axum::body::Body,
    timeout: std::time::Duration,
    sleep: std::pin::Pin<Box<tokio::time::Sleep>>,
    total: std::pin::Pin<Box<tokio::time::Sleep>>,
    expired: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Debug)]
struct BodyInactivity;

impl std::fmt::Display for BodyInactivity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("request body stalled")
    }
}

impl std::error::Error for BodyInactivity {}

impl http_body::Body for InactivityBody {
    type Data = axum::body::Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        use std::future::Future as _;
        use std::task::Poll;

        let this = self.get_mut();
        if this.total.as_mut().poll(cx).is_ready() {
            this.expired
                .store(true, std::sync::atomic::Ordering::Relaxed);
            return Poll::Ready(Some(Err(axum::Error::new(BodyInactivity))));
        }
        match std::pin::Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(frame) => {
                this.sleep = Box::pin(tokio::time::sleep(this.timeout));
                Poll::Ready(frame)
            }
            Poll::Pending => {
                if this.sleep.as_mut().poll(cx).is_ready() {
                    this.expired
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    Poll::Ready(Some(Err(axum::Error::new(BodyInactivity))))
                } else {
                    Poll::Pending
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

/// Bounds the pause between request-body chunks by `timeout`, and the whole
/// body by [`body_deadline`], answering `408 Request Timeout` when a client
/// stalls or trickles mid-body.
///
/// `header_read_timeout` only covers the request head, so without this a POST
/// announcing a body it never sends, or sending one byte per window, would
/// pin its connection and permit forever.
pub(super) async fn enforce_body_inactivity(
    axum::extract::State(timeout): axum::extract::State<HeaderReadTimeout>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;

    let expired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (parts, body) = request.into_parts();
    let body = if http_body::Body::is_end_stream(&body) {
        body
    } else {
        axum::body::Body::new(InactivityBody {
            inner: body,
            timeout: timeout.get(),
            sleep: Box::pin(tokio::time::sleep(timeout.get())),
            total: Box::pin(tokio::time::sleep_until(super::saturating_deadline(
                tokio::time::Instant::now(),
                body_deadline(timeout),
            ))),
            expired: std::sync::Arc::clone(&expired),
        })
    };

    let response = next
        .run(axum::extract::Request::from_parts(parts, body))
        .await;
    if expired.load(std::sync::atomic::Ordering::Relaxed) {
        return axum::http::StatusCode::REQUEST_TIMEOUT.into_response();
    }
    response
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A body trickling one chunk just inside every idle window still
    /// fails once its total deadline passes, flagging the 408.
    #[tokio::test(start_paused = true)]
    async fn test_a_trickling_request_body_expires_at_the_total_deadline() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;

        let idle = HeaderReadTimeout::new(Duration::from_secs(30)).unwrap();
        let chunks = futures::stream::unfold(0_u32, |count| async move {
            tokio::time::sleep(Duration::from_secs(29)).await;
            Some((
                Ok::<_, std::io::Error>(axum::body::Bytes::from_static(b"x")),
                count + 1,
            ))
        });
        let expired = Arc::new(AtomicBool::new(false));
        let mut body = Box::pin(InactivityBody {
            inner: axum::body::Body::from_stream(chunks),
            timeout: idle.get(),
            sleep: Box::pin(tokio::time::sleep(idle.get())),
            total: Box::pin(tokio::time::sleep(body_deadline(idle))),
            expired: Arc::clone(&expired),
        });
        let start = tokio::time::Instant::now();

        let outcome = loop {
            let frame =
                std::future::poll_fn(|cx| http_body::Body::poll_frame(body.as_mut(), cx)).await;
            if let Some(Err(error)) = frame {
                break error;
            }
        };

        assert!(outcome.to_string().contains("stalled"), "{outcome}");
        assert!(expired.load(Ordering::Relaxed));
        assert!(
            start.elapsed() >= Duration::from_mins(2),
            "{:?}",
            start.elapsed()
        );
        assert!(
            start.elapsed() < Duration::from_mins(3),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn test_body_deadline_is_four_idle_windows_with_a_floor_and_never_overflows() {
        use std::time::Duration;

        let deadline = |idle: Duration| body_deadline(HeaderReadTimeout::new(idle).unwrap());
        assert_eq!(deadline(Duration::from_secs(5)), Duration::from_mins(2));
        assert_eq!(deadline(Duration::from_mins(1)), Duration::from_mins(4));
        assert_eq!(deadline(Duration::MAX), Duration::MAX);
    }
}
