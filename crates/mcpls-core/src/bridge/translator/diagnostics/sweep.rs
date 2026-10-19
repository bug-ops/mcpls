//! Resync and re-pull of a tracked file on behalf of its subscribers (#648).
//!
//! The schedule (interval, subscribed files, per-server jobs) belongs to
//! `runtime::disk_sweep`; this module is the unit it runs per file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::Mutex;
use tracing::debug;

use super::{PullSupport, Translator};
use crate::bridge::notifications::DocumentStamp;
use crate::bridge::{ClientPath, NotificationCache, WorkspacePath};
use crate::config::{ServerId, ToolKind};
use crate::error::Error;
use crate::lsp::LspClient;
use crate::util::lock_std;

/// How often the sweep pulls a document state whose pull failed before it
/// leaves the file to the next tool call or edit.
const MAX_FAILED_PULLS_PER_STATE: u8 = 3;

/// What the sweep made of its pulls of one file at one document state.
#[derive(Debug, Clone, Copy)]
enum Attempt {
    /// The server answered; whether the cache stored the report or not, a
    /// pull of the same state would add nothing.
    Answered(DocumentStamp),
    /// The pull failed this many times.
    Failed { state: DocumentStamp, failures: u8 },
}

impl Attempt {
    /// Whether another pull of `current` is pointless.
    fn settles(self, current: DocumentStamp) -> bool {
        match self {
            Self::Answered(state) => state == current,
            Self::Failed { state, failures } => {
                state == current && failures >= MAX_FAILED_PULLS_PER_STATE
            }
        }
    }

    /// The attempt after one more pull of `current`, which `failed` or not.
    fn after(previous: Option<Self>, current: DocumentStamp, failed: bool) -> Self {
        if !failed {
            return Self::Answered(current);
        }
        let failures = match previous {
            Some(Self::Failed { state, failures }) if state == current => failures,
            Some(Self::Failed { .. } | Self::Answered(_)) | None => 0,
        };
        Self::Failed {
            state: current,
            failures: failures.saturating_add(1),
        }
    }
}

/// The document state each file was last pulled at by the sweep.
///
/// A report that is not stored (`unchanged`, partial) leaves no stamp in the
/// cache, so without this memo the file would be pulled again on every tick.
/// A failed pull is retried on the following ticks, at most
/// [`MAX_FAILED_PULLS_PER_STATE`] times for one state: a server busy right
/// after a checkout answers `ContentModified` for a few seconds.
#[derive(Debug, Clone, Default)]
pub struct SweepMemo(Arc<StdMutex<HashMap<PathBuf, Attempt>>>);

impl SweepMemo {
    fn settles(&self, path: &Path, current: DocumentStamp) -> bool {
        lock_std(&self.0)
            .get(path)
            .is_some_and(|attempt| attempt.settles(current))
    }

    fn record(&self, path: &Path, current: DocumentStamp, failed: bool) {
        let mut attempts = lock_std(&self.0);
        let next = Attempt::after(attempts.get(path).copied(), current, failed);
        attempts.insert(path.to_path_buf(), next);
    }

    fn forget(&self, path: &Path) {
        lock_std(&self.0).remove(path);
    }

    /// Forgets every file `keep` rejects, so the memo never outgrows the set
    /// of tracked, subscribed files.
    pub(crate) fn retain(&self, keep: impl Fn(&Path) -> bool) {
        lock_std(&self.0).retain(|path, _| keep(path));
    }
}

/// What sweeping one file did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepStep {
    /// The file was not swept: no running server routes it, or it could not
    /// be opened.
    Skipped,
    /// The document is in sync with the server and nothing was pulled: the
    /// server does not pull, or it was pulled at this state already.
    Synced,
    /// The document was pulled.
    Pulled,
    /// The pull timed out, so the server is not answering; the caller should
    /// leave its remaining files for a later tick.
    TimedOut,
}

impl Translator {
    /// The server the sweep would use for `path`: the one routed for
    /// diagnostics, when it is running.
    ///
    /// Never respawns: a dead, initializing, restarting or failed server is
    /// left to the tool calls that reach it.
    pub(crate) fn sweep_server(&self, path: &Path) -> Option<ServerId> {
        self.sweep_target(path).map(|(id, _client)| id)
    }

    fn sweep_target(&self, path: &Path) -> Option<(ServerId, LspClient)> {
        let (id, client) = self.client_for_file(path, ToolKind::Diagnostics).ok()?;
        self.dead_server_config(&id)
            .is_none()
            .then_some((id, client))
    }

    /// Brings the tracked document at `path` up to date with the disk and
    /// pulls its diagnostics when the pulled slot does not answer the
    /// resulting state yet, which publishes what that changed.
    ///
    /// A pull is sent only when the server is known to answer pulls (it
    /// advertises a provider, or a tool call's probe showed that it answers),
    /// the cache's pulled slot of the file does not stamp the tracker's
    /// current (opening, version), and `memo` has no settled attempt at that
    /// state. A file whose state moved is therefore pulled once, however many
    /// ticks follow.
    pub(crate) async fn sweep_file(
        &self,
        path: &Path,
        memo: &SweepMemo,
        notification_cache: &Mutex<NotificationCache>,
    ) -> SweepStep {
        let Some((server_id, client)) = self.sweep_target(path) else {
            return SweepStep::Skipped;
        };
        if self.document_tracker.opening(path).is_none() {
            return SweepStep::Skipped;
        }
        let Some(validated) = self.revalidated(path).await else {
            memo.forget(path);
            return SweepStep::Skipped;
        };
        let doc = match self
            .open_prepared(server_id, client, validated.as_path())
            .await
        {
            Ok(doc) => doc,
            Err(error) => {
                debug!(path = %path.display(), %error, "disk sweep could not open the file");
                return SweepStep::Skipped;
            }
        };
        let Some(version) = self
            .document_tracker
            .synced_version(doc.path(), doc.server_id())
        else {
            return SweepStep::Skipped;
        };
        let current = DocumentStamp::new(self.document_tracker.opening(doc.path()), version);
        let stored = notification_cache
            .lock()
            .await
            .pulled_stamp(doc.server_id(), doc.uri());
        if !matches!(
            self.pull_support(doc.server_id()),
            PullSupport::Advertised | PullSupport::Answers
        ) || stored == Some(current)
            || memo.settles(path, current)
        {
            return SweepStep::Synced;
        }
        let round = self.pull_round(&doc, notification_cache).await;
        memo.record(path, current, round.failure.is_some());
        match round.failure {
            Some(Error::Timeout(_)) => SweepStep::TimedOut,
            Some(error) => {
                debug!(path = %path.display(), %error, "disk sweep pull failed");
                SweepStep::Pulled
            }
            None => SweepStep::Pulled,
        }
    }

    /// `path` validated as a tool call would, when that leaves it unchanged:
    /// a tracked file that became a symlink out of the workspace roots, or
    /// into another file, is not the document the sweep tracks.
    async fn revalidated(&self, path: &Path) -> Option<WorkspacePath> {
        let client_path = ClientPath::try_from(path.to_path_buf()).ok()?;
        match self.validate_path(&client_path).await {
            Ok(validated) if validated.as_path() == path => Some(validated),
            Ok(_) => {
                debug!(path = %path.display(), "disk sweep skips a tracked file that resolves elsewhere");
                None
            }
            Err(error) => {
                debug!(path = %path.display(), %error, "disk sweep skips a tracked file that fails validation");
                None
            }
        }
    }
}
