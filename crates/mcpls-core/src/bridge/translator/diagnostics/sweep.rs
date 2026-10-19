//! Resync and re-pull of tracked files on behalf of their subscribers (#648,
//! #781, #806).
//!
//! The schedule (interval, subscribed files, companions, per-server jobs)
//! belongs to `runtime::disk_sweep`; this module is the unit it runs per
//! server: [`SweepContext::run`].
//!
//! A job resyncs every file it was given first, which is all the schedule
//! waits for, then hands the subscribed files that are due to a pull pass
//! that runs in the background, one pass per server at a time. A pull that has
//! not answered after [`PULL_PATIENCE`] moves out of the pass, to wait in the
//! background where at most [`MAX_DETACHED_PULLS_PER_SERVER`] may wait per
//! server, so one unanswered pull delays only its own file. Once a pull ran
//! out of patience, the pass waits only [`PULL_GRACE`] for each pull after it,
//! so a file that answers is not kept waiting behind several that do not. When
//! the background slots are full the pull is cancelled without counting as a
//! failure, and the retries wait for a later tick. A pass never delays the
//! resync of the next tick.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use lsp_types::{
    DidSaveTextDocumentNotification, DidSaveTextDocumentParams, TextDocumentIdentifier, Uri,
};
use tokio::sync::Mutex;
use tokio::time::{Instant, timeout};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::debug;

use super::{PullSupport, Translator};
use crate::bridge::notifications::DocumentStamp;
use crate::bridge::state::{ContentDigest, SweepState};
use crate::bridge::translator::routing::PreparedDocument;
use crate::bridge::translator::save_support::SaveSupport;
use crate::bridge::{Access, ClientPath, NotificationCache, WorkspacePath};
use crate::config::{ServerId, ToolKind, WorkspaceTrust};
use crate::error::{Error, ErrorChain};
use crate::lsp::LspClient;
use crate::util::lock_std;

/// Time between two sweeps.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(2);

/// Most subscribed files one sweep visits; the rest follow on later ticks.
pub const MAX_SWEPT_FILES_PER_TICK: usize = 64;

/// Most recently used tracked, unsubscribed files of a server with a
/// subscribed file that count as companions.
pub const MAX_COMPANION_FILES: usize = 256;

/// How long a pull pass waits for the first pull that does not answer before
/// the pull moves to the background.
const PULL_PATIENCE: Duration = SWEEP_INTERVAL;

/// How long a pass waits for each pull after one has run out of patience: the
/// pass has paid for an unanswered pull already, so a file that answers
/// promptly is not kept waiting behind several that do not.
const PULL_GRACE: Duration = Duration::from_millis(250);

/// Most pulls per server that wait in the background past [`PULL_PATIENCE`];
/// the pull a job waits for in the foreground comes on top.
const MAX_DETACHED_PULLS_PER_SERVER: usize = 2;

/// How often the sweep pulls a document state whose pull failed before it
/// leaves the file to the next tool call or edit.
pub const MAX_FAILED_PULLS_PER_STATE: u8 = 3;

/// Memo entries kept for files the sweep no longer visits, so a later visit
/// still compares with what the sweep last saw of them.
const MAX_MEMO_TOMBSTONES: usize = MAX_COMPANION_FILES;

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

/// Why a pull failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureKind {
    /// The server did not answer in time.
    TimedOut,
    /// The server or the connection reported an error.
    Errored,
}

/// How one pull ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PullOutcome {
    Answered,
    Failed(FailureKind),
}

/// Why a file is due for a pull; the order is the order of the pulls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Due {
    /// The file's own document state is new to the sweep.
    Own,
    /// Another file of the server changed since the file was pulled.
    Epoch,
    /// The last pull of the current state failed and may be tried again.
    Retry,
}

/// What the sweep made of its pulls of one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attempt {
    NotPulled,
    /// The server answered; whether the cache stored the report or not, a
    /// pull of the same state would add nothing.
    Answered {
        state: DocumentStamp,
        epoch: SweepEpoch,
    },
    /// The pulls of `state` failed. Timeouts are counted whatever the epoch,
    /// so a file whose server never answers is pulled a bounded number of
    /// times per state; errors are counted per epoch, so a file that failed
    /// fast is pulled again when another file changed.
    Failed {
        state: DocumentStamp,
        epoch: SweepEpoch,
        timeouts: u8,
        errors: u8,
    },
}

impl Attempt {
    /// Whether another pull at `current` and `epoch` is pointless.
    fn settles(self, current: DocumentStamp, epoch: SweepEpoch) -> bool {
        match self {
            Self::NotPulled => false,
            Self::Answered { state, epoch: at } => state == current && at == epoch,
            Self::Failed {
                state,
                epoch: at,
                timeouts,
                errors,
            } => {
                state == current
                    && (timeouts >= MAX_FAILED_PULLS_PER_STATE
                        || (at == epoch && errors >= MAX_FAILED_PULLS_PER_STATE))
            }
        }
    }

    /// Why a pull at `current` and `epoch` is due, if it is.
    fn due(self, current: DocumentStamp, epoch: SweepEpoch) -> Option<Due> {
        match self {
            Self::NotPulled => Some(Due::Own),
            Self::Answered { state, .. } | Self::Failed { state, .. } if state != current => {
                Some(Due::Own)
            }
            Self::Answered { epoch: at, .. } => (at != epoch).then_some(Due::Epoch),
            Self::Failed { .. } => (!self.settles(current, epoch)).then_some(Due::Retry),
        }
    }

    /// The attempt after one more pull at `current` and `epoch` that ended
    /// as `outcome`.
    fn after(self, current: DocumentStamp, epoch: SweepEpoch, outcome: PullOutcome) -> Self {
        let PullOutcome::Failed(kind) = outcome else {
            return Self::Answered {
                state: current,
                epoch,
            };
        };
        let (timeouts, errors) = match self {
            Self::Failed {
                state,
                epoch: at,
                timeouts,
                errors,
            } if state == current => (timeouts, if at == epoch { errors } else { 0 }),
            Self::NotPulled | Self::Answered { .. } | Self::Failed { .. } => (0, 0),
        };
        let (timeouts, errors) = match kind {
            FailureKind::TimedOut => (timeouts.saturating_add(1), errors),
            FailureKind::Errored => (timeouts, errors.saturating_add(1)),
        };
        Self::Failed {
            state: current,
            epoch,
            timeouts,
            errors,
        }
    }
}

/// Whether a content change moved a file's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Content {
    /// The text is the one the sweep saw before (a reopen of the same file).
    Same,
    /// The text differs, or the sweep cannot tell.
    Different,
}

/// How a visit found a file's document state relative to the last visit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    /// The state is the one the memo holds.
    Unchanged,
    /// The file was edited or opened again.
    Moved(Content),
}

impl Change {
    /// Whether the file's text changed, which is when `didSave` goes out and
    /// the server's epoch moves.
    const fn content_changed(self) -> bool {
        matches!(self, Self::Moved(Content::Different))
    }
}

/// Identifies one claim on a file's pull, so a claim that outlived its memo
/// entry cannot touch the entry of a later one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClaimId(u64);

/// Where an outstanding pull waits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placement {
    /// A job waits for it.
    Inline,
    /// It waits in the background, counted against its server's cap.
    Detached,
}

/// The state of a file's pull.
#[derive(Debug, Clone, Copy)]
enum FilePull {
    Idle(Attempt),
    /// A claim holds the file; `previous` is what it restores when dropped.
    Outstanding {
        previous: Attempt,
        claim: ClaimId,
        placement: Placement,
    },
}

#[derive(Debug, Clone)]
struct FileMemo {
    server: ServerId,
    seen: DocumentStamp,
    /// The digest of the text the server was told when `seen` was recorded;
    /// `None` when the sweep could not read it, which compares as different.
    digest: Option<ContentDigest>,
    pull: FilePull,
    /// Whether a `didSave` for the last change is still to be sent.
    save: SaveDebt,
    /// The tick of the last visit.
    last_tick: u64,
}

/// Whether the server still has to be told that a changed file was saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SaveDebt {
    Settled,
    /// The `didSave` timed out, so the next visit sends it again.
    Owed,
}

/// How long one budget of epoch-driven pulls lasts: one sweep interval.
const EPOCH_PULL_WINDOW: Duration = SWEEP_INTERVAL;

/// Most pulls per server and [`EPOCH_PULL_WINDOW`] that are due only because
/// another file of the server changed, so a mass change (a branch switch, a
/// rewritten companion) spreads the re-pulls of up to [`MAX_SWEPT_FILES_PER_TICK`]
/// subscribed files over several ticks instead of sending them together.
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
    next_claim: u64,
    tick: u64,
    /// Servers with a pull pass running.
    pulling: HashSet<ServerId>,
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
/// A visit that finds the text of a file changed (the sweep's own resync or a
/// tool call's) advances the epoch of the file's server, which makes every
/// subscribed file of that server due for a pull.
///
/// Entries of files the sweep stopped visiting stay as tombstones, the most
/// recent [`MAX_MEMO_TOMBSTONES`] of them, so a file resynced by a tool call
/// in the meantime is still compared with what the sweep last saw. Past that
/// cap a path is new to the sweep again and gets no `didSave` for a change
/// made before its next visit.
#[derive(Debug, Clone, Default)]
pub struct SweepMemo(Arc<StdMutex<MemoState>>);

/// The rights and duties of one outstanding pull of a file.
#[derive(Debug)]
struct ClaimTicket {
    path: PathBuf,
    id: ClaimId,
    state: DocumentStamp,
    epoch: SweepEpoch,
}

/// The exclusive right to pull one file. Finishing it records how the pull
/// ended; dropping it unfinished (the pull was cancelled) restores the file's
/// earlier attempt.
#[derive(Debug)]
#[must_use = "dropping the claim abandons the pull"]
struct PullClaim {
    memo: SweepMemo,
    ticket: Option<ClaimTicket>,
}

impl PullClaim {
    fn finish(mut self, outcome: PullOutcome) {
        if let Some(ticket) = self.ticket.take() {
            self.memo.complete(&ticket, outcome);
        }
    }
}

impl Drop for PullClaim {
    fn drop(&mut self) {
        if let Some(ticket) = self.ticket.take() {
            self.memo.abort(&ticket);
        }
    }
}

/// The right to run the pull pass of one server; there is one at a time.
#[derive(Debug)]
#[must_use = "dropping the pass lets the next one start"]
struct PullPass {
    memo: SweepMemo,
    server: ServerId,
}

impl Drop for PullPass {
    fn drop(&mut self) {
        lock_std(&self.memo.0).pulling.remove(&self.server);
    }
}

/// What claiming a file's pull found.
#[derive(Debug)]
enum ClaimResult {
    Claimed(PullClaim),
    /// Another pull of the file is outstanding.
    Outstanding,
    /// A pull is no longer due.
    Settled,
}

impl SweepMemo {
    /// Takes the right to run a pull pass for `server`, when none runs.
    fn begin_pull_pass(&self, server: &ServerId) -> Option<PullPass> {
        lock_std(&self.0)
            .pulling
            .insert(server.clone())
            .then(|| PullPass {
                memo: self.clone(),
                server: server.clone(),
            })
    }

    /// Starts a new tick, which the visits that follow are stamped with.
    pub(crate) fn begin_tick(&self) {
        let mut state = lock_std(&self.0);
        state.tick = state.tick.saturating_add(1);
    }

    /// Records that `path` of `server` is at `after`, reached from `before`
    /// (the state just before the visit's resync), and advances the server's
    /// epoch when the text changed.
    fn observe(
        &self,
        server: &ServerId,
        path: &Path,
        before: Option<SweepState>,
        after: SweepState,
    ) -> Change {
        let mut state = lock_std(&self.0);
        let tick = state.tick;
        let (previous, previous_digest) = if let Some(memo) = state.files.get_mut(path) {
            let previous = (Some(memo.seen), memo.digest);
            memo.seen = after.stamp();
            memo.digest = after.digest();
            memo.server = server.clone();
            memo.last_tick = tick;
            previous
        } else {
            state.files.insert(
                path.to_path_buf(),
                FileMemo {
                    server: server.clone(),
                    seen: after.stamp(),
                    digest: after.digest(),
                    pull: FilePull::Idle(Attempt::NotPulled),
                    save: SaveDebt::Settled,
                    last_tick: tick,
                },
            );
            (
                before.map(SweepState::stamp),
                before.and_then(SweepState::digest),
            )
        };
        let change = match previous {
            Some(previous) if previous != after.stamp() => {
                let same = previous_digest.is_some() && previous_digest == after.digest();
                Change::Moved(if same {
                    Content::Same
                } else {
                    Content::Different
                })
            }
            Some(_) | None => Change::Unchanged,
        };
        if change.content_changed() {
            let epoch = state.epochs.entry(server.clone()).or_default();
            *epoch = epoch.next();
        }
        drop(state);
        change
    }

    /// Takes the `didSave` a timed-out visit left owing for `path`.
    fn take_save_debt(&self, path: &Path) -> SaveDebt {
        lock_std(&self.0)
            .files
            .get_mut(path)
            .map_or(SaveDebt::Settled, |memo| {
                std::mem::replace(&mut memo.save, SaveDebt::Settled)
            })
    }

    /// Records that the `didSave` of `path` is still to be sent.
    fn owe_save(&self, path: &Path) {
        if let Some(memo) = lock_std(&self.0).files.get_mut(path) {
            memo.save = SaveDebt::Owed;
        }
    }

    fn epoch(&self, server: &ServerId) -> SweepEpoch {
        lock_std(&self.0)
            .epochs
            .get(server)
            .copied()
            .unwrap_or_default()
    }

    /// Why `path` is due for a pull at `current` and `epoch`, if it is and no
    /// pull of it is outstanding.
    fn due(&self, path: &Path, current: DocumentStamp, epoch: SweepEpoch) -> Option<Due> {
        let pull = lock_std(&self.0).files.get(path)?.pull;
        match pull {
            FilePull::Idle(attempt) => attempt.due(current, epoch),
            FilePull::Outstanding { .. } => None,
        }
    }

    /// Records that a pull of `current` would add nothing: the cache holds
    /// the report for it already.
    fn record_answered(&self, path: &Path, current: DocumentStamp, epoch: SweepEpoch) {
        if let Some(memo) = lock_std(&self.0).files.get_mut(path)
            && matches!(memo.pull, FilePull::Idle(_))
        {
            memo.pull = FilePull::Idle(Attempt::Answered {
                state: current,
                epoch,
            });
        }
    }

    /// Takes the right to pull `path` at `current` and `epoch`.
    fn claim_pull(&self, path: &Path, current: DocumentStamp, epoch: SweepEpoch) -> ClaimResult {
        let mut state = lock_std(&self.0);
        let id = ClaimId(state.next_claim);
        let Some(memo) = state.files.get_mut(path) else {
            return ClaimResult::Settled;
        };
        let previous = match memo.pull {
            FilePull::Outstanding { .. } => return ClaimResult::Outstanding,
            FilePull::Idle(attempt) if attempt.due(current, epoch).is_none() => {
                return ClaimResult::Settled;
            }
            FilePull::Idle(attempt) => attempt,
        };
        memo.pull = FilePull::Outstanding {
            previous,
            claim: id,
            placement: Placement::Inline,
        };
        state.next_claim = state.next_claim.saturating_add(1);
        drop(state);
        ClaimResult::Claimed(PullClaim {
            memo: self.clone(),
            ticket: Some(ClaimTicket {
                path: path.to_path_buf(),
                id,
                state: current,
                epoch,
            }),
        })
    }

    /// Whether `server` may take one more pull into the background.
    #[cfg(test)]
    fn detached_slot_free(&self, server: &ServerId) -> bool {
        Self::detached_count(&lock_std(&self.0), server) < MAX_DETACHED_PULLS_PER_SERVER
    }

    fn detached_count(state: &MemoState, server: &ServerId) -> usize {
        state
            .files
            .values()
            .filter(|memo| {
                memo.server == *server
                    && matches!(
                        memo.pull,
                        FilePull::Outstanding {
                            placement: Placement::Detached,
                            ..
                        }
                    )
            })
            .count()
    }

    /// Moves the pull `claim` holds into the background when its server has a
    /// slot for it; otherwise hands the claim back unchanged.
    fn detach(&self, claim: PullClaim) -> Result<PullClaim, PullClaim> {
        let mut state = lock_std(&self.0);
        let Some(ticket) = &claim.ticket else {
            return Err(claim);
        };
        let Some(server) = state
            .files
            .get(&ticket.path)
            .map(|memo| memo.server.clone())
        else {
            return Err(claim);
        };
        if Self::detached_count(&state, &server) >= MAX_DETACHED_PULLS_PER_SERVER {
            return Err(claim);
        }
        if let Some(memo) = state.files.get_mut(&ticket.path)
            && let FilePull::Outstanding {
                claim: held,
                placement,
                ..
            } = &mut memo.pull
            && *held == ticket.id
        {
            *placement = Placement::Detached;
        }
        drop(state);
        Ok(claim)
    }

    fn complete(&self, ticket: &ClaimTicket, outcome: PullOutcome) {
        if let Some(memo) = lock_std(&self.0).files.get_mut(&ticket.path)
            && let FilePull::Outstanding {
                previous, claim, ..
            } = memo.pull
            && claim == ticket.id
        {
            memo.pull = FilePull::Idle(previous.after(ticket.state, ticket.epoch, outcome));
        }
    }

    fn abort(&self, ticket: &ClaimTicket) {
        if let Some(memo) = lock_std(&self.0).files.get_mut(&ticket.path)
            && let FilePull::Outstanding {
                previous, claim, ..
            } = memo.pull
            && claim == ticket.id
        {
            memo.pull = FilePull::Idle(previous);
        }
    }

    /// Forgets `path` unless a pull of it is outstanding.
    fn forget(&self, path: &Path) {
        let mut state = lock_std(&self.0);
        if state
            .files
            .get(path)
            .is_some_and(|memo| matches!(memo.pull, FilePull::Idle(_)))
        {
            state.files.remove(path);
        }
    }

    /// Keeps the files `keep` accepts, the files with an outstanding pull and
    /// the most recently visited [`MAX_MEMO_TOMBSTONES`] of the others, so the
    /// memo never outgrows the set the sweep visits by more than that.
    pub(crate) fn retain(&self, keep: impl Fn(&Path) -> bool) {
        let mut state = lock_std(&self.0);
        let MemoState {
            files,
            epochs,
            budgets,
            ..
        } = &mut *state;
        let mut tombstones: Vec<(u64, PathBuf)> = files
            .iter()
            .filter(|(path, memo)| !keep(path) && matches!(memo.pull, FilePull::Idle(_)))
            .map(|(path, memo)| (memo.last_tick, path.clone()))
            .collect();
        tombstones.sort_unstable_by(|(a, _), (b, _)| b.cmp(a));
        for (_, path) in tombstones.into_iter().skip(MAX_MEMO_TOMBSTONES) {
            files.remove(&path);
        }
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
    /// The document was pulled and the server answered or failed.
    Pulled,
    /// The pull has not answered in time and waits in the background.
    Detached,
    /// The pull has not answered in time and the server has no slot for it
    /// in the background, so it was cancelled; it is not counted as a failure
    /// and the file stays due.
    GaveUp,
    /// The pull is left for a later tick: the job already paid for an
    /// unanswered pull at full cap.
    Deferred,
    /// The resync did not finish in time, so the server's queue is wedged;
    /// the job ended and its remaining files wait for a later tick.
    TimedOut,
}

/// A tracked file brought up to date with the disk, as the sweep left it.
struct Resynced {
    server: ServerId,
    uri: Uri,
    current: DocumentStamp,
}

/// How a step that waits on the server's queue ended.
enum Bounded<T> {
    /// The file is not swept: no running server routes it, or it could not
    /// be opened.
    Skipped,
    /// The step did not finish within the request timeout.
    TimedOut,
    Done(T),
}

/// A subscribed file that may be pulled after the resyncs of its job.
pub(super) struct PullCandidate {
    path: PathBuf,
    resynced: Resynced,
}

/// What one pull pass has met so far.
#[derive(Debug, Default)]
struct PassState {
    /// A pull ran out of patience, so the pulls after it get only a grace.
    paid_patience: bool,
    /// A pull ran out of patience with the server's background slots full.
    at_cap: bool,
}

impl PassState {
    const fn patience(&self) -> Duration {
        if self.paid_patience {
            PULL_GRACE
        } else {
            PULL_PATIENCE
        }
    }
}

type BoxedPull = Pin<Box<dyn Future<Output = PullOutcome> + Send>>;

/// Everything a per-server sweep job needs, cheap to clone into each job.
#[derive(Clone)]
pub struct SweepContext {
    translator: Arc<Translator>,
    notification_cache: Arc<Mutex<NotificationCache>>,
    memo: SweepMemo,
    pulls: TaskTracker,
    stop: CancellationToken,
}

impl SweepContext {
    /// A context whose background pulls are tracked by `pulls` and end when
    /// `stop` fires.
    pub(crate) const fn new(
        translator: Arc<Translator>,
        notification_cache: Arc<Mutex<NotificationCache>>,
        memo: SweepMemo,
        pulls: TaskTracker,
        stop: CancellationToken,
    ) -> Self {
        Self {
            translator,
            notification_cache,
            memo,
            pulls,
            stop,
        }
    }

    pub(crate) const fn memo(&self) -> &SweepMemo {
        &self.memo
    }

    pub(crate) const fn pulls(&self) -> &TaskTracker {
        &self.pulls
    }

    /// Sweeps `visits`, all of one server: resyncs every file, then pulls the
    /// subscribed files that are due, and returns what became of each visit
    /// that was reached, in order.
    ///
    /// A [`Interest::Companion`] file is resynced with [`Access::Probe`]: its
    /// LRU position stays as tool calls left it and an evicted file is not
    /// opened again. A resync that found the text changed sends `didSave` when
    /// the server advertises it and the workspace is trusted, so servers that
    /// produce diagnostics only on save re-run.
    ///
    /// A pull is sent only when the server is known to answer pulls (it
    /// advertises a provider, or a tool call's probe showed that it answers),
    /// and either the file's own state or its server's epoch moved since the
    /// last pull, and the cache's pulled slot does not already answer the
    /// state. A file whose state moved is therefore pulled once, however many
    /// ticks follow.
    #[cfg(test)]
    pub(crate) async fn run(&self, visits: Vec<(PathBuf, Interest)>) -> Vec<SweepStep> {
        let (mut steps, candidates) = self.resync_pass(&visits).await;
        self.pull_due(candidates, &mut steps).await;
        log_steps(&visits, &steps);
        steps
    }

    /// As [`Self::run`], but returns once the files are resynced: the pulls
    /// run in the background, unless the server's previous pull pass is still
    /// running, in which case the files due stay due for a later tick.
    pub(crate) async fn run_job(&self, visits: Vec<(PathBuf, Interest)>) {
        let (mut steps, candidates) = self.resync_pass(&visits).await;
        // TODO(#809): a skipped pass drops its candidates, which wait a window cycle.
        let pass = candidates
            .first()
            .and_then(|(_, candidate)| self.memo.begin_pull_pass(&candidate.resynced.server));
        let Some(pass) = pass else {
            log_steps(&visits, &steps);
            return;
        };
        let context = self.clone();
        self.pulls.spawn(async move {
            let _pass = pass;
            tokio::select! {
                biased;
                () = context.stop.cancelled() => {}
                () = context.pull_due(candidates, &mut steps) => {}
            }
            log_steps(&visits, &steps);
        });
    }

    /// Resyncs every visit and collects the subscribed files that may be
    /// pulled, with the index of their step.
    pub(super) async fn resync_pass(
        &self,
        visits: &[(PathBuf, Interest)],
    ) -> (Vec<SweepStep>, Vec<(usize, PullCandidate)>) {
        let mut steps = Vec::with_capacity(visits.len());
        let mut candidates = Vec::new();
        for (path, interest) in visits {
            if self.stop.is_cancelled() {
                break;
            }
            let access = match interest {
                Interest::Subscribed => Access::Use,
                Interest::Companion => Access::Probe,
            };
            match self.translator.resync(path, access, &self.memo).await {
                Bounded::Skipped => steps.push(SweepStep::Skipped),
                Bounded::TimedOut => {
                    steps.push(SweepStep::TimedOut);
                    break;
                }
                Bounded::Done(resynced) => {
                    if *interest == Interest::Subscribed
                        && matches!(
                            self.translator.pull_support(&resynced.server),
                            PullSupport::Advertised | PullSupport::Answers
                        )
                    {
                        candidates.push((
                            steps.len(),
                            PullCandidate {
                                path: path.clone(),
                                resynced,
                            },
                        ));
                    }
                    steps.push(SweepStep::Synced);
                }
            }
        }
        (steps, candidates)
    }

    /// Pulls the candidates that are due: new states first, then files
    /// another file's change made stale, then failed pulls to retry.
    pub(super) async fn pull_due(
        &self,
        candidates: Vec<(usize, PullCandidate)>,
        steps: &mut [SweepStep],
    ) {
        let mut due: Vec<(Due, SweepEpoch, usize, PullCandidate)> = candidates
            .into_iter()
            .filter_map(|(index, candidate)| {
                let epoch = self.memo.epoch(&candidate.resynced.server);
                let why = self
                    .memo
                    .due(&candidate.path, candidate.resynced.current, epoch)?;
                Some((why, epoch, index, candidate))
            })
            .collect();
        due.sort_by_key(|(why, ..)| *why);
        let mut pass = PassState::default();
        for (why, epoch, index, candidate) in due {
            if self.stop.is_cancelled() {
                return;
            }
            if pass.at_cap && why == Due::Retry {
                set_step(steps, index, SweepStep::Deferred);
                continue;
            }
            let step = self.pull_one(why, epoch, &candidate, &mut pass).await;
            set_step(steps, index, step);
            if step == SweepStep::TimedOut {
                return;
            }
        }
    }

    /// Pulls one file that is `why` due. A pull that does not answer within
    /// the pass's patience ([`PULL_PATIENCE`] until one pull ran out of it,
    /// [`PULL_GRACE`] after) moves to the background if its server has a slot;
    /// otherwise it is cancelled, counted as a timeout, and `at_cap` set.
    async fn pull_one(
        &self,
        why: Due,
        epoch: SweepEpoch,
        candidate: &PullCandidate,
        pass: &mut PassState,
    ) -> SweepStep {
        let PullCandidate { path, resynced } = candidate;
        if why == Due::Own {
            let stored = self
                .notification_cache
                .lock()
                .await
                .pulled_stamp(&resynced.server, &resynced.uri);
            if stored == Some(resynced.current) {
                self.memo.record_answered(path, resynced.current, epoch);
                return SweepStep::Synced;
            }
        }
        let ClaimResult::Claimed(claim) = self.memo.claim_pull(path, resynced.current, epoch)
        else {
            return SweepStep::Synced;
        };
        let doc = match self
            .translator
            .open_bounded(path, Access::Use, &self.memo)
            .await
        {
            Bounded::Done((doc, _)) => doc,
            Bounded::Skipped => return SweepStep::Skipped,
            Bounded::TimedOut => return SweepStep::TimedOut,
        };
        // A change since this job's resync is observed, and pulled, by the next visit.
        if self
            .translator
            .document_tracker
            .stamp(doc.path(), doc.server_id())
            != Some(resynced.current)
        {
            return SweepStep::Synced;
        }
        if why == Due::Epoch && !self.memo.take_epoch_pull(&resynced.server, Instant::now()) {
            return SweepStep::Synced;
        }
        let mut pull: BoxedPull = Box::pin(run_pull(
            Arc::clone(&self.translator),
            Arc::clone(&self.notification_cache),
            doc,
            path.clone(),
        ));
        let waited = timeout(pass.patience(), &mut pull).await;
        if waited.is_err() {
            pass.paid_patience = true;
        }
        match waited {
            Ok(outcome) => {
                claim.finish(outcome);
                SweepStep::Pulled
            }
            Err(_elapsed) => match self.memo.detach(claim) {
                Ok(claim) => {
                    self.spawn_detached(pull, claim);
                    SweepStep::Detached
                }
                Err(claim) => {
                    drop((pull, claim));
                    pass.at_cap = true;
                    SweepStep::GaveUp
                }
            },
        }
    }

    /// Lets `pull` finish in the background, until the sweep stops.
    fn spawn_detached(&self, pull: BoxedPull, claim: PullClaim) {
        let stop = self.stop.clone();
        self.pulls.spawn(async move {
            tokio::select! {
                biased;
                () = stop.cancelled() => {}
                outcome = pull => claim.finish(outcome),
            }
        });
    }
}

fn set_step(steps: &mut [SweepStep], index: usize, step: SweepStep) {
    if let Some(slot) = steps.get_mut(index) {
        *slot = step;
    }
}

fn log_steps(visits: &[(PathBuf, Interest)], steps: &[SweepStep]) {
    for ((path, interest), step) in visits.iter().zip(steps) {
        debug!(path = %path.display(), ?interest, ?step, "disk sweep visited a file");
    }
}

/// Pulls `doc`, which keeps the file in flight until the pull ends.
async fn run_pull(
    translator: Arc<Translator>,
    notification_cache: Arc<Mutex<NotificationCache>>,
    doc: PreparedDocument,
    path: PathBuf,
) -> PullOutcome {
    let round = translator.pull_round(&doc, &notification_cache).await;
    match round.failure {
        None => PullOutcome::Answered,
        Some(Error::Timeout(_)) => PullOutcome::Failed(FailureKind::TimedOut),
        Some(error) => {
            debug!(path = %path.display(), error = %ErrorChain(&error), "disk sweep pull failed");
            PullOutcome::Failed(FailureKind::Errored)
        }
    }
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

    /// Opens the tracked document at `path` for the sweep, bounded by the
    /// server's request timeout, and returns it with the state the server
    /// had of it before.
    ///
    /// The document is validated as a tool call would, which fails for a
    /// file that became a symlink out of the workspace roots or into another
    /// file, and is never opened again once the tracker evicted it.
    async fn open_bounded(
        &self,
        path: &Path,
        access: Access,
        memo: &SweepMemo,
    ) -> Bounded<(PreparedDocument, Option<SweepState>)> {
        let Some((server_id, client)) = self.sweep_target(path) else {
            return Bounded::Skipped;
        };
        if self.document_tracker.opening(path).is_none() {
            return Bounded::Skipped;
        }
        let before = self.document_tracker.sweep_state(path, &server_id);
        let Some(validated) = self.revalidated(path).await else {
            memo.forget(path);
            return Bounded::Skipped;
        };
        let limit = client.request_timeout();
        match timeout(
            limit,
            self.open_prepared(server_id, client, &validated, access),
        )
        .await
        {
            Ok(Ok(doc)) => Bounded::Done((doc, before)),
            Ok(Err(error)) => {
                debug!(path = %path.display(), error = %ErrorChain(&error), "disk sweep could not open the file");
                Bounded::Skipped
            }
            Err(_elapsed) => {
                debug!(path = %path.display(), "disk sweep timed out opening the file");
                Bounded::TimedOut
            }
        }
    }

    /// Brings the tracked document at `path` up to date with the disk and
    /// records what changed.
    async fn resync(&self, path: &Path, access: Access, memo: &SweepMemo) -> Bounded<Resynced> {
        let (doc, before) = match self.open_bounded(path, access, memo).await {
            Bounded::Done(opened) => opened,
            Bounded::Skipped => return Bounded::Skipped,
            Bounded::TimedOut => return Bounded::TimedOut,
        };
        let Some(after) = self
            .document_tracker
            .sweep_state(doc.path(), doc.server_id())
        else {
            return Bounded::Skipped;
        };
        let change = memo.observe(doc.server_id(), path, before, after);
        let owed = memo.take_save_debt(path);
        if change.content_changed() || owed == SaveDebt::Owed {
            let limit = doc.client().request_timeout();
            if timeout(limit, self.send_did_save(&doc)).await.is_err() {
                debug!(path = %path.display(), "disk sweep timed out sending didSave");
                memo.owe_save(path);
                return Bounded::TimedOut;
            }
        }
        Bounded::Done(Resynced {
            server: doc.server_id().clone(),
            uri: doc.uri().clone(),
            current: after.stamp(),
        })
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
        if let Err(error) = doc
            .client()
            .notify_typed::<DidSaveTextDocumentNotification>(params)
            .await
        {
            debug!(path = %doc.path().display(), error = %ErrorChain(&error), "disk sweep could not send didSave");
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
                debug!(path = %path.display(), error = %ErrorChain(&error), "disk sweep skips a tracked file that fails validation");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;
    use crate::bridge::DocumentVersion;

    fn stamp(version: i32) -> DocumentStamp {
        DocumentStamp::new(None, DocumentVersion::new(version))
    }

    fn state(version: i32, text: &str) -> SweepState {
        SweepState::Current {
            stamp: stamp(version),
            digest: ContentDigest::of(text),
        }
    }

    fn rust() -> ServerId {
        ServerId::from_static("rust")
    }

    fn memo_with(paths: &[&str]) -> SweepMemo {
        let memo = SweepMemo::default();
        for path in paths {
            memo.observe(&rust(), Path::new(path), None, state(1, path));
        }
        memo
    }

    const fn epoch(n: u64) -> SweepEpoch {
        SweepEpoch(n)
    }

    fn failed(kind: FailureKind) -> PullOutcome {
        PullOutcome::Failed(kind)
    }

    fn claim_of(memo: &SweepMemo, path: &str) -> PullClaim {
        match memo.claim_pull(Path::new(path), stamp(1), epoch(0)) {
            ClaimResult::Claimed(claim) => claim,
            other => panic!("claim of {path}: {other:?}"),
        }
    }

    #[test]
    fn a_state_that_keeps_timing_out_gets_the_cap_of_pulls_whatever_the_epoch() {
        let mut attempt = Attempt::NotPulled;
        let mut pulls = 0_u8;
        let mut at = 0;
        while attempt.due(stamp(1), epoch(at)).is_some() {
            attempt = attempt.after(stamp(1), epoch(at), failed(FailureKind::TimedOut));
            pulls += 1;
            at += 1;
            assert!(pulls <= MAX_FAILED_PULLS_PER_STATE, "never settles");
        }
        assert_eq!(pulls, MAX_FAILED_PULLS_PER_STATE);
    }

    #[test]
    fn errors_are_counted_per_epoch() {
        let mut attempt = Attempt::NotPulled;
        for _ in 0..MAX_FAILED_PULLS_PER_STATE {
            attempt = attempt.after(stamp(1), epoch(0), failed(FailureKind::Errored));
        }
        assert_eq!(attempt.due(stamp(1), epoch(0)), None);
        assert_eq!(attempt.due(stamp(1), epoch(1)), Some(Due::Retry));
        attempt = attempt.after(stamp(1), epoch(1), failed(FailureKind::Errored));
        assert_eq!(attempt.due(stamp(1), epoch(1)), Some(Due::Retry));
    }

    #[test]
    fn a_new_state_or_a_moved_epoch_makes_a_pull_due() {
        let answered = Attempt::NotPulled.after(stamp(1), epoch(0), PullOutcome::Answered);
        assert_eq!(answered.due(stamp(1), epoch(0)), None);
        assert_eq!(answered.due(stamp(1), epoch(1)), Some(Due::Epoch));
        assert_eq!(answered.due(stamp(2), epoch(0)), Some(Due::Own));
        assert_eq!(Attempt::NotPulled.due(stamp(1), epoch(0)), Some(Due::Own));
    }

    #[test]
    fn pulls_are_ordered_own_then_epoch_then_retry() {
        assert!(Due::Own < Due::Epoch && Due::Epoch < Due::Retry);
    }

    #[test]
    fn a_text_change_moves_the_epoch_and_an_identical_reopen_does_not() {
        let memo = memo_with(&["/w/a.rs"]);
        let path = Path::new("/w/a.rs");

        let reopened = memo.observe(&rust(), path, None, state(2, "/w/a.rs"));
        assert_eq!(reopened, Change::Moved(Content::Same));
        assert_eq!(memo.epoch(&rust()), epoch(0));

        let edited = memo.observe(&rust(), path, None, state(3, "edited"));
        assert_eq!(edited, Change::Moved(Content::Different));
        assert_eq!(memo.epoch(&rust()), epoch(1));

        let unreadable = memo.observe(&rust(), path, None, SweepState::Lagging { stamp: stamp(4) });
        assert_eq!(unreadable, Change::Moved(Content::Different));
        assert_eq!(memo.epoch(&rust()), epoch(2));
    }

    #[test]
    fn a_file_the_memo_never_saw_is_compared_with_the_state_before_the_resync() {
        let memo = SweepMemo::default();
        let unchanged = memo.observe(&rust(), Path::new("/w/a.rs"), None, state(1, "a"));
        assert_eq!(unchanged, Change::Unchanged);

        let edited = memo.observe(
            &rust(),
            Path::new("/w/b.rs"),
            Some(state(1, "old")),
            state(2, "new"),
        );
        assert_eq!(edited, Change::Moved(Content::Different));
        let same = memo.observe(
            &rust(),
            Path::new("/w/c.rs"),
            Some(state(1, "x")),
            state(2, "x"),
        );
        assert_eq!(same, Change::Moved(Content::Same));
    }

    #[test]
    fn one_claim_holds_a_file_and_dropping_it_restores_the_attempt() {
        let memo = memo_with(&["/w/a.rs"]);
        let path = Path::new("/w/a.rs");
        let claim = claim_of(&memo, "/w/a.rs");
        assert_eq!(memo.due(path, stamp(1), epoch(0)), None);
        assert_matches!(
            memo.claim_pull(path, stamp(1), epoch(0)),
            ClaimResult::Outstanding
        );

        drop(claim);

        assert_eq!(memo.due(path, stamp(1), epoch(0)), Some(Due::Own));
    }

    #[test]
    fn finishing_a_claim_records_the_outcome() {
        let memo = memo_with(&["/w/a.rs"]);
        let claim = claim_of(&memo, "/w/a.rs");

        claim.finish(PullOutcome::Answered);

        assert_matches!(
            memo.claim_pull(Path::new("/w/a.rs"), stamp(1), epoch(0)),
            ClaimResult::Settled
        );
    }

    #[test]
    fn a_claim_that_outlived_its_entry_leaves_the_next_entry_alone() {
        let memo = memo_with(&["/w/a.rs"]);
        let path = Path::new("/w/a.rs");
        let old = claim_of(&memo, "/w/a.rs");
        lock_std(&memo.0).files.remove(path);
        memo.observe(&rust(), path, None, state(1, "again"));

        old.finish(PullOutcome::Answered);

        assert_eq!(memo.due(path, stamp(1), epoch(0)), Some(Due::Own));
    }

    #[test]
    fn forgetting_and_trimming_keep_a_file_with_an_outstanding_pull() {
        let memo = memo_with(&["/w/a.rs", "/w/b.rs"]);
        let held = Path::new("/w/a.rs");
        let _claim = claim_of(&memo, "/w/a.rs");

        memo.forget(held);
        memo.forget(Path::new("/w/b.rs"));
        memo.retain(|_| false);

        assert_eq!(memo.tracked_files(), 1);
        assert!(lock_std(&memo.0).files.contains_key(held));
    }

    #[test]
    fn trimming_keeps_the_most_recent_tombstones_only() {
        let memo = SweepMemo::default();
        let name = |n: usize| format!("/w/f{n:04}.rs");
        for n in 0..(MAX_MEMO_TOMBSTONES + 5) {
            memo.begin_tick();
            memo.observe(&rust(), Path::new(&name(n)), None, state(1, "x"));
        }

        memo.retain(|_| false);

        assert_eq!(memo.tracked_files(), MAX_MEMO_TOMBSTONES);
        let files = &lock_std(&memo.0).files;
        assert!(!files.contains_key(Path::new(&name(0))));
        assert!(files.contains_key(Path::new(&name(MAX_MEMO_TOMBSTONES + 4))));
    }

    #[test]
    fn a_server_has_a_bounded_number_of_background_pulls() {
        let paths: Vec<String> = (0..=MAX_DETACHED_PULLS_PER_SERVER)
            .map(|n| format!("/w/f{n}.rs"))
            .collect();
        let memo = memo_with(&paths.iter().map(String::as_str).collect::<Vec<_>>());
        let mut claims: Vec<PullClaim> = paths.iter().map(|path| claim_of(&memo, path)).collect();
        let last = claims.pop().unwrap();

        let detached: Vec<_> = claims
            .into_iter()
            .map(|claim| memo.detach(claim).expect("a slot is free"))
            .collect();

        assert!(!memo.detached_slot_free(&rust()));
        let refused = memo.detach(last).expect_err("the cap is full");
        drop(refused);
        drop(detached);
        assert!(memo.detached_slot_free(&rust()));
    }

    #[test]
    fn a_save_that_timed_out_stays_owed_until_it_is_taken() {
        let memo = memo_with(&["/w/a.rs"]);
        let path = Path::new("/w/a.rs");
        assert_eq!(memo.take_save_debt(path), SaveDebt::Settled);

        memo.owe_save(path);

        assert_eq!(memo.take_save_debt(path), SaveDebt::Owed);
        assert_eq!(memo.take_save_debt(path), SaveDebt::Settled);
    }

    #[test]
    fn one_pull_pass_runs_per_server() {
        let memo = SweepMemo::default();
        let first = memo.begin_pull_pass(&rust()).expect("no pass runs");
        assert!(memo.begin_pull_pass(&rust()).is_none());
        assert!(
            memo.begin_pull_pass(&ServerId::from_static("other"))
                .is_some()
        );
        drop(first);
        assert!(memo.begin_pull_pass(&rust()).is_some());
    }
}
