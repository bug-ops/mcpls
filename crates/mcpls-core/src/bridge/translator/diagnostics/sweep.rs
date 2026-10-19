//! Resync and re-pull of a tracked file on behalf of its subscribers (#648,
//! #781).
//!
//! The schedule (interval, subscribed files, companions, per-server jobs)
//! belongs to `runtime::disk_sweep`; this module is the unit it runs per file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use lsp_types::{
    DidSaveTextDocumentNotification, DidSaveTextDocumentParams, TextDocumentIdentifier,
};
use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::debug;

use super::{PullSupport, Translator};
use crate::bridge::notifications::DocumentStamp;
use crate::bridge::translator::routing::PreparedDocument;
use crate::bridge::translator::save_support::SaveSupport;
use crate::bridge::{Access, ClientPath, NotificationCache, WorkspacePath};
use crate::config::{ServerId, ToolKind, WorkspaceTrust};
use crate::error::Error;
use crate::lsp::LspClient;
use crate::util::lock_std;

/// How often the sweep pulls a document state whose pull failed before it
/// leaves the file to the next tool call or edit.
const MAX_FAILED_PULLS_PER_STATE: u8 = 3;

/// A counter of the content changes the sweep observed on one server's files.
///
/// A change to any tracked file may change the diagnostics of another file of
/// the same server, so a subscribed file is pulled again when this moved since
/// its last pull.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct SweepEpoch(u64);

impl SweepEpoch {
    const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

/// What the sweep made of its pulls of one file at one document state.
#[derive(Debug, Clone, Copy)]
enum Attempt {
    /// The server answered; whether the cache stored the report or not, a
    /// pull of the same state would add nothing.
    Answered {
        state: DocumentStamp,
        epoch: SweepEpoch,
    },
    /// The pull failed this many times.
    Failed {
        state: DocumentStamp,
        epoch: SweepEpoch,
        failures: u8,
    },
}

impl Attempt {
    /// Whether another pull at `current` and `epoch` is pointless.
    fn settles(self, current: DocumentStamp, epoch: SweepEpoch) -> bool {
        match self {
            Self::Answered { state, epoch: at } => state == current && at == epoch,
            Self::Failed {
                state,
                epoch: at,
                failures,
            } => state == current && at == epoch && failures >= MAX_FAILED_PULLS_PER_STATE,
        }
    }

    /// The attempt after one more pull at `current` and `epoch`, which
    /// `failed` or not.
    fn after(
        previous: Option<Self>,
        current: DocumentStamp,
        epoch: SweepEpoch,
        failed: bool,
    ) -> Self {
        if !failed {
            return Self::Answered {
                state: current,
                epoch,
            };
        }
        let failures = match previous {
            Some(Self::Failed {
                state,
                epoch: at,
                failures,
            }) if state == current && at == epoch => failures,
            Some(Self::Failed { .. } | Self::Answered { .. }) | None => 0,
        };
        Self::Failed {
            state: current,
            epoch,
            failures: failures.saturating_add(1),
        }
    }
}

/// How a visit found a file's document state relative to the last visit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    /// The state is the one the memo holds.
    None,
    /// The content changed within one opening of the file.
    Edited,
    /// The file was opened again.
    Reopened,
}

/// What one visit to a file observed.
#[derive(Debug, Clone, Copy)]
struct Observation {
    /// No earlier visit is remembered.
    first_sight: bool,
    change: Change,
}

impl Observation {
    /// Whether this visit itself found the file new or moved, as opposed to
    /// another file of the server having moved.
    fn is_own(self) -> bool {
        self.first_sight || self.change != Change::None
    }
}

#[derive(Debug, Clone)]
struct FileMemo {
    server: ServerId,
    seen: DocumentStamp,
    pull: Option<Attempt>,
}

/// How long one budget of epoch-driven pulls lasts: one sweep interval.
const EPOCH_PULL_WINDOW: Duration = Duration::from_secs(2);

/// Most pulls per server and [`EPOCH_PULL_WINDOW`] that are due only because
/// another file of the server changed, so a mass change (a branch switch, a
/// rewritten companion) spreads the re-pulls of up to 64 subscribed files over
/// several ticks instead of sending them together.
const MAX_EPOCH_PULLS_PER_WINDOW: u8 = 8;

/// The epoch-driven pulls a server spent in its current window.
#[derive(Debug, Clone, Copy)]
struct PullBudget {
    window_start: Instant,
    spent: u8,
}

#[derive(Debug, Default)]
struct MemoState {
    files: HashMap<PathBuf, FileMemo>,
    epochs: HashMap<ServerId, SweepEpoch>,
    budgets: HashMap<ServerId, PullBudget>,
}

/// The document state each file was last seen at by the sweep, and the pull
/// attempted for it.
///
/// A report that is not stored (`unchanged`, partial) leaves no stamp in the
/// cache, so without this memo the file would be pulled again on every tick.
/// A failed pull is retried on the following ticks, at most
/// [`MAX_FAILED_PULLS_PER_STATE`] times for one state: a server busy right
/// after a checkout answers `ContentModified` for a few seconds.
///
/// A visit that finds the document state moved (the sweep's own resync or a
/// tool call's) advances the epoch of the file's server, which makes every
/// subscribed file of that server due for a pull.
#[derive(Debug, Clone, Default)]
pub struct SweepMemo(Arc<StdMutex<MemoState>>);

impl SweepMemo {
    /// Records that `path` of `server` is at `after`, reached from `before`
    /// (the state just before the visit's resync), and advances the server's
    /// epoch when that is a change.
    fn observe(
        &self,
        server: &ServerId,
        path: &Path,
        before: Option<DocumentStamp>,
        after: DocumentStamp,
    ) -> Observation {
        let mut state = lock_std(&self.0);
        let (first_sight, previous) = if let Some(memo) = state.files.get_mut(path) {
            let previous = Some(memo.seen);
            memo.seen = after;
            memo.server = server.clone();
            (false, previous)
        } else {
            state.files.insert(
                path.to_path_buf(),
                FileMemo {
                    server: server.clone(),
                    seen: after,
                    pull: None,
                },
            );
            (true, before)
        };
        let change = match previous {
            None => Change::None,
            Some(previous) if previous == after => Change::None,
            Some(previous) if previous.same_opening(after) => Change::Edited,
            Some(_) => Change::Reopened,
        };
        if change != Change::None {
            let epoch = state.epochs.entry(server.clone()).or_default();
            *epoch = epoch.next();
        }
        drop(state);
        Observation {
            first_sight,
            change,
        }
    }

    fn epoch(&self, server: &ServerId) -> SweepEpoch {
        lock_std(&self.0)
            .epochs
            .get(server)
            .copied()
            .unwrap_or_default()
    }

    fn settles(&self, path: &Path, current: DocumentStamp, epoch: SweepEpoch) -> bool {
        lock_std(&self.0)
            .files
            .get(path)
            .and_then(|memo| memo.pull)
            .is_some_and(|attempt| attempt.settles(current, epoch))
    }

    fn record(&self, path: &Path, current: DocumentStamp, epoch: SweepEpoch, failed: bool) {
        if let Some(memo) = lock_std(&self.0).files.get_mut(path) {
            memo.pull = Some(Attempt::after(memo.pull, current, epoch, failed));
        }
    }

    fn forget(&self, path: &Path) {
        lock_std(&self.0).files.remove(path);
    }

    /// Forgets every file `keep` rejects, so the memo never outgrows the set
    /// of subscribed and companion files.
    pub(crate) fn retain(&self, keep: impl Fn(&Path) -> bool) {
        let mut state = lock_std(&self.0);
        let MemoState {
            files,
            epochs,
            budgets,
        } = &mut *state;
        files.retain(|path, _| keep(path));
        epochs.retain(|server, _| files.values().any(|memo| memo.server == *server));
        budgets.retain(|server, _| files.values().any(|memo| memo.server == *server));
        drop(state);
    }

    /// Takes one epoch-driven pull of `server` out of its budget for the
    /// window `now` falls in; `false` when the window has none left.
    pub(super) fn take_epoch_pull(&self, server: &ServerId, now: Instant) -> bool {
        let mut state = lock_std(&self.0);
        let budget = state.budgets.entry(server.clone()).or_insert(PullBudget {
            window_start: now,
            spent: 0,
        });
        if now.duration_since(budget.window_start) >= EPOCH_PULL_WINDOW {
            *budget = PullBudget {
                window_start: now,
                spent: 0,
            };
        }
        let available = budget.spent < MAX_EPOCH_PULLS_PER_WINDOW;
        if available {
            budget.spent = budget.spent.saturating_add(1);
        }
        drop(state);
        available
    }

    #[cfg(test)]
    pub(crate) fn tracked_files(&self) -> usize {
        lock_std(&self.0).files.len()
    }
}

/// Why the sweep visits a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interest {
    /// A session subscribes to the file: it is resynced and pulled.
    Subscribed,
    /// Nobody subscribes to the file, but its server serves a subscribed one:
    /// it is resynced without touching its LRU position and never pulled, so a
    /// change to it reaches subscribed files through the server's epoch.
    Companion,
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

/// A tracked file brought up to date with the disk, as the sweep left it.
struct Resynced {
    doc: PreparedDocument,
    current: DocumentStamp,
    seen: Observation,
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

    /// Brings the tracked document at `path` up to date with the disk and,
    /// for a [`Interest::Subscribed`] file, pulls its diagnostics when the
    /// pulled slot does not answer the resulting state yet, which publishes
    /// what that changed.
    ///
    /// A resync that found the content changed (edited or reopened) sends
    /// `didSave` when the server advertises it and the workspace is trusted,
    /// so servers that produce diagnostics only on save re-run.
    /// A pull is sent only when the server is known to answer pulls (it
    /// advertises a provider, or a tool call's probe showed that it answers),
    /// and either the file's own state or its server's epoch moved since the
    /// last pull, and the cache's pulled slot does not already answer the
    /// state. A file whose state moved is therefore pulled once, however many
    /// ticks follow.
    ///
    /// A [`Interest::Companion`] file is resynced with [`Access::Probe`]: its
    /// LRU position stays as tool calls left it and an evicted file is not
    /// opened again.
    pub(crate) async fn sweep_file(
        &self,
        path: &Path,
        interest: Interest,
        memo: &SweepMemo,
        notification_cache: &Mutex<NotificationCache>,
    ) -> SweepStep {
        let access = match interest {
            Interest::Subscribed => Access::Use,
            Interest::Companion => Access::Probe,
        };
        let Some(resynced) = self.resync(path, access, memo).await else {
            return SweepStep::Skipped;
        };
        match interest {
            Interest::Subscribed => {
                self.pull_if_due(path, resynced, memo, notification_cache)
                    .await
            }
            Interest::Companion => SweepStep::Synced,
        }
    }

    async fn resync(&self, path: &Path, access: Access, memo: &SweepMemo) -> Option<Resynced> {
        let (server_id, client) = self.sweep_target(path)?;
        self.document_tracker.opening(path)?;
        let before = self.stamp_of(path, &server_id);
        let Some(validated) = self.revalidated(path).await else {
            memo.forget(path);
            return None;
        };
        let doc = match self
            .open_prepared(server_id, client, &validated, access)
            .await
        {
            Ok(doc) => doc,
            Err(error) => {
                debug!(path = %path.display(), %error, "disk sweep could not open the file");
                return None;
            }
        };
        let current = self.stamp_of(doc.path(), doc.server_id())?;
        let seen = memo.observe(doc.server_id(), path, before, current);
        if seen.change != Change::None {
            self.send_did_save(&doc).await;
        }
        Some(Resynced { doc, current, seen })
    }

    async fn pull_if_due(
        &self,
        path: &Path,
        resynced: Resynced,
        memo: &SweepMemo,
        notification_cache: &Mutex<NotificationCache>,
    ) -> SweepStep {
        let Resynced { doc, current, seen } = resynced;
        let epoch = memo.epoch(doc.server_id());
        if !matches!(
            self.pull_support(doc.server_id()),
            PullSupport::Advertised | PullSupport::Answers
        ) || memo.settles(path, current, epoch)
        {
            return SweepStep::Synced;
        }
        let stored = notification_cache
            .lock()
            .await
            .pulled_stamp(doc.server_id(), doc.uri());
        if stored == Some(current) && seen.is_own() {
            memo.record(path, current, epoch, false);
            return SweepStep::Synced;
        }
        if !seen.is_own() && !memo.take_epoch_pull(doc.server_id(), Instant::now()) {
            return SweepStep::Synced;
        }
        let round = self.pull_round(&doc, notification_cache).await;
        memo.record(path, current, epoch, round.failure.is_some());
        match round.failure {
            Some(Error::Timeout(_)) => SweepStep::TimedOut,
            Some(error) => {
                debug!(path = %path.display(), %error, "disk sweep pull failed");
                SweepStep::Pulled
            }
            None => SweepStep::Pulled,
        }
    }

    /// The document state `server` was last told about `path`, if it was told.
    fn stamp_of(&self, path: &Path, server: &ServerId) -> Option<DocumentStamp> {
        let version = self.document_tracker.synced_version(path, server)?;
        Some(DocumentStamp::new(
            self.document_tracker.opening(path),
            version,
        ))
    }

    /// Tells the server of `doc` that the document was saved, when it asked
    /// for `didSave`, with the text when it asked for that too.
    ///
    /// Never sent in an untrusted workspace: a save makes servers such as
    /// rust-analyzer run build scripts and proc macros.
    async fn send_did_save(&self, doc: &PreparedDocument) {
        if matches!(self.workspace_trust, WorkspaceTrust::Untrusted(_)) {
            return;
        }
        let support = lock_std(&self.servers).save_support(doc.server_id());
        let text = match support {
            SaveSupport::NotAdvertised => return,
            SaveSupport::WithoutText => None,
            SaveSupport::WithText => {
                let Some(text) = self.document_tracker.text(doc.path()) else {
                    return;
                };
                Some(text)
            }
        };
        let params = DidSaveTextDocumentParams {
            text_document: TextDocumentIdentifier {
                uri: doc.uri().clone(),
            },
            text,
        };
        let sent = async {
            let notification =
                LspClient::outbound_notification::<DidSaveTextDocumentNotification>(params)?;
            doc.client()
                .reserve_notification()
                .await?
                .send(notification);
            Ok::<(), Error>(())
        }
        .await;
        if let Err(error) = sent {
            debug!(path = %doc.path().display(), %error, "disk sweep could not send didSave");
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
