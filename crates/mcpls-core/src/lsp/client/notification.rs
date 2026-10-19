//! Outbound LSP notifications: their encoding and the one place they are
//! enqueued on the command channel, whether sent at once, without waiting, or
//! through a reserved slot.

use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use super::{ClientCommand, LspClient};
use crate::error::{Error, Result};

/// A notification encoded and ready to enqueue.
#[derive(Debug)]
pub struct OutboundNotification {
    method: String,
    params: Option<Value>,
}

impl OutboundNotification {
    /// Encodes `params` for `method`; `null` params are omitted from the frame.
    pub(super) fn new<P: Serialize>(method: &str, params: P) -> Result<Self> {
        Ok(Self {
            method: method.to_owned(),
            params: LspClient::omit_null_params(serde_json::to_value(params)?),
        })
    }

    fn into_command(self) -> ClientCommand {
        ClientCommand::SendNotification {
            method: self.method,
            params: self.params,
        }
    }
}

impl LspClient {
    /// Encodes `params` for the notification `N`, ready to enqueue on a
    /// reserved slot.
    ///
    /// # Errors
    ///
    /// Returns an error if `params` cannot be serialized.
    pub(crate) fn outbound_notification<N>(params: N::Params) -> Result<OutboundNotification>
    where
        N: lsp_types::Notification,
    {
        OutboundNotification::new(N::METHOD.as_str(), params)
    }

    /// Waits for room on the command channel and enqueues `notification`.
    pub(super) async fn send_notification(&self, notification: OutboundNotification) -> Result<()> {
        self.command_tx
            .send(notification.into_command())
            .await
            .map_err(|_| Error::ServerTerminated)
    }

    /// Enqueues `notification` without waiting; a full or closed queue is an
    /// error for the caller to log.
    pub(super) fn try_send_notification(
        &self,
        notification: OutboundNotification,
    ) -> std::result::Result<(), TrySendError<ClientCommand>> {
        self.command_tx.try_send(notification.into_command())
    }

    /// Waits for room for one notification and reserves it.
    ///
    /// # Errors
    ///
    /// [`Error::ServerTerminated`] if the server has shut down.
    pub(crate) async fn reserve_notification(&self) -> Result<NotificationPermit<'_>> {
        let permit = self
            .command_tx
            .reserve()
            .await
            .map_err(|_| Error::ServerTerminated)?;
        Ok(NotificationPermit(permit))
    }

    /// Waits for room for two notifications and reserves both, in send order.
    ///
    /// # Errors
    ///
    /// [`Error::ServerTerminated`] if the server has shut down or its queue
    /// cannot hold two.
    pub(crate) async fn reserve_notification_pair(
        &self,
    ) -> Result<(NotificationPermit<'_>, NotificationPermit<'_>)> {
        let mut permits = self
            .command_tx
            .reserve_many(2)
            .await
            .map_err(|_| Error::ServerTerminated)?;
        match (permits.next(), permits.next()) {
            (Some(first), Some(second)) => {
                Ok((NotificationPermit(first), NotificationPermit(second)))
            }
            _ => Err(Error::ServerTerminated),
        }
    }
}

/// A reserved slot on the command channel for one notification.
///
/// Reserving is the only part that can wait, and it is cancel-safe: nothing is
/// sent until [`Self::send`], which is synchronous. A caller that must enqueue
/// several notifications and record the outcome in one step reserves first.
#[derive(Debug)]
#[must_use = "a reserved slot does nothing until used"]
pub struct NotificationPermit<'a>(mpsc::Permit<'a, ClientCommand>);

impl NotificationPermit<'_> {
    /// Enqueues `notification` on the reserved slot.
    pub(crate) fn send(self, notification: OutboundNotification) {
        self.0.send(notification.into_command());
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;
    use crate::config::LspServerConfig;

    #[tokio::test(start_paused = true)]
    async fn a_reserved_pair_sends_both_in_order() {
        let (command_tx, mut command_rx) = mpsc::channel(2);
        let client = LspClient {
            command_tx,
            ..LspClient::new(LspServerConfig::rust_analyzer())
        };
        let (first, second) = client.reserve_notification_pair().await.unwrap();
        let one = LspClient::outbound_notification::<lsp_types::ExitNotification>(()).unwrap();
        let two = LspClient::outbound_notification::<lsp_types::ExitNotification>(()).unwrap();
        first.send(one);
        second.send(two);
        for _ in 0..2 {
            assert_matches!(
                command_rx.recv().await,
                Some(ClientCommand::SendNotification { method, .. }) if method == "exit"
            );
        }
    }

    #[tokio::test]
    async fn a_pair_does_not_fit_a_queue_of_one() {
        let (command_tx, _command_rx) = mpsc::channel(1);
        let client = LspClient {
            command_tx,
            ..LspClient::new(LspServerConfig::rust_analyzer())
        };
        assert_matches!(
            client.reserve_notification_pair().await,
            Err(Error::ServerTerminated)
        );
    }

    #[test]
    fn null_params_are_omitted_from_the_frame() {
        let notification =
            LspClient::outbound_notification::<lsp_types::ExitNotification>(()).unwrap();
        assert_eq!(notification.params, None);
        let notification =
            OutboundNotification::new("custom/probe", serde_json::json!({})).unwrap();
        assert_eq!(notification.params, Some(serde_json::json!({})));
    }
}
