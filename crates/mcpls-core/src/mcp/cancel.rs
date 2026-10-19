//! Stopping a `tools/call` handler when its client goes away.
//!
//! rmcp cancels the per-request token on `notifications/cancelled` but does not
//! abort the handler, and it does not cancel it at transport EOF at all. The
//! handler is therefore raced against both signals; dropping it cancels every
//! LSP request it has outstanding (see `LspClient`'s in-flight guard).

use std::future::Future;
use std::time::Duration;

use rmcp::RoleServer;
use rmcp::model::{CallToolResponse, CallToolResult, ContentBlock};
use rmcp::service::Peer;
use tokio_util::sync::CancellationToken;

/// How often a running handler checks whether its connection has closed.
///
/// rmcp exposes no future for "the transport closed", only
/// [`Peer::is_transport_closed`], which turns true when the serve loop ends.
/// That is after rmcp's own drain of in-flight handlers (5 s at 3.5.1), so a
/// handler outlives a closed connection by at most that drain plus this
/// period.
pub(super) const PEER_CLOSE_POLL: Duration = Duration::from_millis(250);

/// Why a handler was stopped before it finished. The text is what the log and
/// the placeholder response carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(super) enum CallStop {
    /// The client sent `notifications/cancelled`, or the serve loop was
    /// cancelled.
    #[error("request cancelled (client cancellation or server shutdown)")]
    ClientCancelled,
    /// The connection ended.
    #[error("connection closed")]
    ConnectionClosed,
}

impl CallStop {
    /// The tool-level error result that stands in for the call's response.
    ///
    /// rmcp discards it for a call the client cancelled and the transport is
    /// gone when the connection closed; only a call still pending when the
    /// serve loop is cancelled has it delivered, in place of a protocol error
    /// that would be logged as a failure.
    pub(super) fn into_response(self) -> CallToolResponse {
        CallToolResult::error(vec![ContentBlock::text(self.to_string())]).into()
    }
}

/// Resolves once `peer`'s transport has closed.
pub(super) async fn peer_closed(peer: &Peer<RoleServer>) {
    while !peer.is_transport_closed() {
        tokio::time::sleep(PEER_CLOSE_POLL).await;
    }
}

/// Runs `handler` until it finishes, the request token fires or `closed`
/// resolves.
///
/// The signals are checked first, in that order, so a call cancelled before
/// it starts never polls its handler: no LSP request is issued and no document
/// is opened for it. When a signal wins, `handler` is dropped, which cancels
/// the LSP requests it had outstanding.
pub(super) async fn run_cancellable<T>(
    request: &CancellationToken,
    closed: impl Future<Output = ()>,
    handler: impl Future<Output = T>,
) -> Result<T, CallStop> {
    tokio::select! {
        biased;
        () = request.cancelled() => Err(CallStop::ClientCancelled),
        () = closed => Err(CallStop::ConnectionClosed),
        output = handler => Ok(output),
    }
}

#[cfg(test)]
mod wire_tests;

#[cfg(test)]
mod tests {
    use std::assert_matches;
    use std::future::pending;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    struct Polled(Arc<AtomicBool>);

    impl Polled {
        async fn never_finishes(self) {
            self.0.store(true, Ordering::SeqCst);
            pending::<()>().await;
        }
    }

    #[tokio::test]
    async fn a_finished_handler_returns_its_output() {
        let token = CancellationToken::new();
        let outcome = run_cancellable(&token, pending(), async { 7 }).await;
        assert_eq!(outcome, Ok(7));
    }

    #[tokio::test]
    async fn a_token_cancelled_up_front_never_polls_the_handler() {
        let token = CancellationToken::new();
        token.cancel();
        let polled = Arc::new(AtomicBool::new(false));
        let outcome = run_cancellable(
            &token,
            pending(),
            Polled(Arc::clone(&polled)).never_finishes(),
        )
        .await;
        assert_matches!(outcome, Err(CallStop::ClientCancelled));
        assert!(!polled.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_closed_connection_stops_the_handler() {
        let token = CancellationToken::new();
        let polled = Arc::new(AtomicBool::new(false));
        let outcome = run_cancellable(
            &token,
            std::future::ready(()),
            Polled(Arc::clone(&polled)).never_finishes(),
        )
        .await;
        assert_matches!(outcome, Err(CallStop::ConnectionClosed));
        assert!(!polled.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_token_fired_mid_flight_drops_the_handler() {
        struct DropFlag(Arc<AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let token = CancellationToken::new();
        let dropped = Arc::new(AtomicBool::new(false));
        let handler = {
            let flag = DropFlag(Arc::clone(&dropped));
            async move {
                let _flag = flag;
                pending::<()>().await;
            }
        };
        let canceller = token.clone();
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            canceller.cancel();
        });
        let outcome = run_cancellable(&token, pending(), handler).await;
        assert_matches!(outcome, Err(CallStop::ClientCancelled));
        assert!(dropped.load(Ordering::SeqCst));
    }
}
