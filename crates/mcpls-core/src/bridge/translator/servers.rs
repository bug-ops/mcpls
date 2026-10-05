//! Per-server state of the [`Translator`](super::Translator), one value per server.
//!
//! A server's status, respawn bookkeeping and restart bookkeeping live in one
//! [`ServerSlot`], read and replaced under one lock, so no reader can see a
//! client next to the wrong server, or a server that is neither registered
//! nor expected. The translator-wide lifecycle is one [`Phase`].

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Mutex;
use tokio::task::AbortHandle;

use super::respawn::RespawnBackoff;
use super::restart::RestartGeneration;
use crate::config::ServerId;
use crate::error::ServerSpawnFailure;
use crate::lsp::{LspClient, LspServer};

/// What a running slot holds.
///
/// Production builds only have [`Self::Process`], whose client is the
/// server's own, so the two can never disagree. The test-only variants keep
/// fixtures that register a bare client, or a client next to a stub server,
/// working.
#[derive(Debug)]
#[cfg_attr(
    test,
    allow(
        clippy::large_enum_variant,
        reason = "the test-only ClientOnly variant holds just a client"
    )
)]
pub(super) enum Backend {
    /// A spawned server and, through it, its client.
    Process(LspServer),
    /// A client with no server: capabilities unknown, never respawned.
    #[cfg(test)]
    ClientOnly(LspClient),
    /// A routing client next to a server that supplies capabilities and
    /// lifecycle.
    #[cfg(test)]
    Split {
        client: LspClient,
        server: LspServer,
    },
}

impl Backend {
    /// The client requests are routed to.
    pub(super) const fn client(&self) -> &LspClient {
        match self {
            Self::Process(server) => server.client(),
            #[cfg(test)]
            Self::ClientOnly(client) | Self::Split { client, .. } => client,
        }
    }

    /// The server, when there is one.
    #[allow(
        clippy::unnecessary_wraps,
        reason = "the test-only ClientOnly variant has no server"
    )]
    pub(super) const fn server(&self) -> Option<&LspServer> {
        match self {
            Self::Process(server) => Some(server),
            #[cfg(test)]
            Self::ClientOnly(_) => None,
            #[cfg(test)]
            Self::Split { server, .. } => Some(server),
        }
    }

    /// The server, mutably, when there is one.
    #[allow(
        clippy::unnecessary_wraps,
        reason = "the test-only ClientOnly variant has no server"
    )]
    pub(super) const fn server_mut(&mut self) -> Option<&mut LspServer> {
        match self {
            Self::Process(server) => Some(server),
            #[cfg(test)]
            Self::ClientOnly(_) => None,
            #[cfg(test)]
            Self::Split { server, .. } => Some(server),
        }
    }
}

/// Where one server stands.
#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "one value per configured server; boxing the backend buys nothing"
)]
pub(super) enum ServerStatus {
    /// Configured and applicable, not registered yet. Keeps the failure it
    /// had before, if any, so clearing the expectation does not lose it.
    Expected {
        prior_failure: Option<ServerSpawnFailure>,
    },
    /// Registered and serving.
    Running(Backend),
    /// Taken out for a manual restart; its backend is held by the restart.
    /// Reads as expected, but startup settlement and expectation clearing
    /// leave it alone, so a restart in flight is never mistaken for a
    /// pending startup.
    Restarting,
    /// Failed to start; terminal until mcpls restarts.
    Failed(ServerSpawnFailure),
    /// Shut down; its client is kept so late callers fail with
    /// `ServerTerminated` from the dead connection.
    Stopped(LspClient),
}

/// Manual-restart bookkeeping of one server.
#[derive(Debug, Default)]
pub(super) struct RestartState {
    /// Count of completed manual restarts.
    pub(super) generation: RestartGeneration,
    /// When the last manual restart attempt started or completed.
    pub(super) last_attempt: Option<Instant>,
}

/// Everything the translator tracks for one server.
#[derive(Debug)]
pub(super) struct ServerSlot {
    status: ServerStatus,
    /// Single-flight lock for respawn and restart of this server.
    pub(super) respawn_lock: Arc<Mutex<()>>,
    /// Crash-loop backoff of automatic respawns.
    pub(super) backoff: Option<RespawnBackoff>,
    pub(super) restart: RestartState,
    /// The task consuming this server's notification lanes.
    pub(super) notification_task: Option<AbortHandle>,
}

impl ServerStatus {
    /// Whether the server is yet to register: pending startup or restarting.
    const fn awaits_registration(&self) -> bool {
        matches!(self, Self::Expected { .. } | Self::Restarting)
    }
}

impl ServerSlot {
    fn new(status: ServerStatus) -> Self {
        Self {
            status,
            respawn_lock: Arc::new(Mutex::new(())),
            backoff: None,
            restart: RestartState::default(),
            notification_task: None,
        }
    }
}

/// The translator's server slots, keyed by routing identity.
#[derive(Debug, Default)]
pub(super) struct Servers(HashMap<ServerId, ServerSlot>);

impl Servers {
    pub(super) fn get(&self, id: &ServerId) -> Option<&ServerSlot> {
        self.0.get(id)
    }

    pub(super) fn get_mut(&mut self, id: &ServerId) -> Option<&mut ServerSlot> {
        self.0.get_mut(id)
    }

    /// Every slot id, in no order.
    pub(super) fn ids(&self) -> impl Iterator<Item = &ServerId> {
        self.0.keys()
    }

    /// The client of a running or stopped server.
    pub(super) fn client(&self, id: &ServerId) -> Option<LspClient> {
        match &self.0.get(id)?.status {
            ServerStatus::Running(backend) => Some(backend.client().clone()),
            ServerStatus::Stopped(client) => Some(client.clone()),
            ServerStatus::Expected { .. } | ServerStatus::Restarting | ServerStatus::Failed(_) => {
                None
            }
        }
    }

    /// The clients of every running or stopped server.
    pub(super) fn clients(&self) -> impl Iterator<Item = &LspClient> {
        self.0.values().filter_map(|slot| match &slot.status {
            ServerStatus::Running(backend) => Some(backend.client()),
            ServerStatus::Stopped(client) => Some(client),
            ServerStatus::Expected { .. } | ServerStatus::Restarting | ServerStatus::Failed(_) => {
                None
            }
        })
    }

    /// The server of a running slot that has one.
    pub(super) fn server(&self, id: &ServerId) -> Option<&LspServer> {
        match &self.0.get(id)?.status {
            ServerStatus::Running(backend) => backend.server(),
            _ => None,
        }
    }

    /// The server of a running slot that has one, mutably.
    pub(super) fn server_mut(&mut self, id: &ServerId) -> Option<&mut LspServer> {
        match &mut self.0.get_mut(id)?.status {
            ServerStatus::Running(backend) => backend.server_mut(),
            _ => None,
        }
    }

    /// Every running server with its id.
    pub(super) fn running_servers(&self) -> impl Iterator<Item = (&ServerId, &LspServer)> {
        self.0.iter().filter_map(|(id, slot)| match &slot.status {
            ServerStatus::Running(backend) => backend.server().map(|server| (id, server)),
            _ => None,
        })
    }

    pub(super) fn is_expected(&self, id: &ServerId) -> bool {
        self.0
            .get(id)
            .is_some_and(|slot| slot.status.awaits_registration())
    }

    /// Whether any server is still expected to register.
    pub(super) fn any_expected(&self) -> bool {
        self.0
            .values()
            .any(|slot| slot.status.awaits_registration())
    }

    /// The ids of every expected server.
    pub(super) fn expected(&self) -> HashSet<ServerId> {
        self.0
            .iter()
            .filter(|(_, slot)| slot.status.awaits_registration())
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// The recorded startup failure of `id`.
    pub(super) fn failure(&self, id: &ServerId) -> Option<&ServerSpawnFailure> {
        match &self.0.get(id)?.status {
            ServerStatus::Failed(failure)
            | ServerStatus::Expected {
                prior_failure: Some(failure),
            } => Some(failure),
            _ => None,
        }
    }

    /// Every recorded startup failure, ordered by routing identity.
    pub(super) fn failures(&self) -> Vec<ServerSpawnFailure> {
        let mut failures: Vec<ServerSpawnFailure> = self
            .0
            .keys()
            .filter_map(|id| self.failure(id).cloned())
            .collect();
        failures.sort_by(|a, b| a.server_id.as_str().cmp(b.server_id.as_str()));
        failures
    }

    /// Marks exactly the servers of `set` as expected: absent and failed ones
    /// become expected (a failed one remembers its failure), a running one is
    /// unchanged, and an expected one outside `set` stops being expected.
    pub(super) fn set_expected(&mut self, set: &HashSet<ServerId>) {
        for id in set {
            match self.0.get_mut(id) {
                None => {
                    self.0.insert(
                        id.clone(),
                        ServerSlot::new(ServerStatus::Expected {
                            prior_failure: None,
                        }),
                    );
                }
                Some(slot) => {
                    if let ServerStatus::Failed(failure) = &slot.status {
                        slot.status = ServerStatus::Expected {
                            prior_failure: Some(failure.clone()),
                        };
                    }
                }
            }
        }
        self.clear_expected_except(set);
    }

    /// Stops expecting every server.
    pub(super) fn clear_expected(&mut self) {
        self.clear_expected_except(&HashSet::new());
    }

    fn clear_expected_except(&mut self, keep: &HashSet<ServerId>) {
        self.0.retain(|id, slot| {
            if keep.contains(id) {
                return true;
            }
            match &mut slot.status {
                ServerStatus::Expected {
                    prior_failure: Some(failure),
                } => {
                    slot.status = ServerStatus::Failed(failure.clone());
                    true
                }
                ServerStatus::Expected {
                    prior_failure: None,
                } => false,
                _ => true,
            }
        });
    }

    /// Notes `failure` for an expected server without ending its expectation,
    /// so lookups keep reporting it as initializing until the router has been
    /// re-derived; [`Self::record_failure`] then settles it.
    pub(super) fn announce_failure(&mut self, failure: &ServerSpawnFailure) {
        if let Some(slot) = self.0.get_mut(&failure.server_id)
            && matches!(slot.status, ServerStatus::Expected { .. })
        {
            slot.status = ServerStatus::Expected {
                prior_failure: Some(failure.clone()),
            };
        }
    }

    /// Records `failure` for its server unless that server is running. An
    /// expected server becomes failed. Returns whether the server was
    /// expected.
    pub(super) fn record_failure(&mut self, failure: &ServerSpawnFailure) -> bool {
        match self.0.get_mut(&failure.server_id) {
            None => {
                self.0.insert(
                    failure.server_id.clone(),
                    ServerSlot::new(ServerStatus::Failed(failure.clone())),
                );
                false
            }
            Some(slot) => match &slot.status {
                ServerStatus::Expected { .. } => {
                    slot.status = ServerStatus::Failed(failure.clone());
                    true
                }
                ServerStatus::Failed(_) => {
                    slot.status = ServerStatus::Failed(failure.clone());
                    false
                }
                ServerStatus::Running(_) | ServerStatus::Restarting | ServerStatus::Stopped(_) => {
                    tracing::debug!(
                        id = %failure.server_id,
                        "startup failure ignored: the server is registered"
                    );
                    false
                }
            },
        }
    }

    /// Records that every config in `configs` that is not registered or
    /// already failed died with its init task.
    pub(super) fn fail_unsettled(&mut self, configs: impl IntoIterator<Item = ServerSpawnFailure>) {
        for failure in configs {
            let settled = matches!(
                self.0.get(&failure.server_id).map(|slot| &slot.status),
                Some(
                    ServerStatus::Running(_)
                        | ServerStatus::Restarting
                        | ServerStatus::Stopped(_)
                        | ServerStatus::Failed(_)
                        | ServerStatus::Expected {
                            prior_failure: Some(_)
                        }
                )
            );
            if !settled {
                self.record_failure(&failure);
            }
        }
    }

    /// Registers `backend` under `id`, replacing whatever the slot held.
    ///
    /// Returns whether the server was expected, and the running backend it
    /// displaced, so the caller drops it after releasing the guard.
    pub(super) fn register(&mut self, id: ServerId, backend: Backend) -> Registered {
        if let Some(slot) = self.0.get_mut(&id) {
            let was_expected = matches!(slot.status, ServerStatus::Expected { .. });
            let displaced =
                match std::mem::replace(&mut slot.status, ServerStatus::Running(backend)) {
                    ServerStatus::Running(previous) => Some(previous),
                    _ => None,
                };
            return Registered {
                was_expected,
                displaced,
            };
        }
        self.0
            .insert(id, ServerSlot::new(ServerStatus::Running(backend)));
        Registered {
            was_expected: false,
            displaced: None,
        }
    }

    /// Takes the backend of a running `id` out for a restart, leaving the slot
    /// `Restarting` so no caller finds the server in neither state.
    pub(super) fn take_for_restart(&mut self, id: &ServerId) -> Option<Backend> {
        let slot = self.0.get_mut(id)?;
        if !matches!(slot.status, ServerStatus::Running(_)) {
            return None;
        }
        match std::mem::replace(&mut slot.status, ServerStatus::Restarting) {
            ServerStatus::Running(backend) => Some(backend),
            other => {
                slot.status = other;
                None
            }
        }
    }

    /// Puts a backend taken by [`Self::take_for_restart`] back, unless the
    /// slot was settled by someone else meanwhile.
    ///
    /// Returns the backend when the slot was settled by someone else, so the
    /// caller drops it after releasing the guard.
    pub(super) fn restore(&mut self, id: &ServerId, backend: Backend) -> Option<Backend> {
        match self.0.get_mut(id) {
            Some(slot) if matches!(slot.status, ServerStatus::Restarting) => {
                slot.status = ServerStatus::Running(backend);
                None
            }
            None => {
                self.0
                    .insert(id.clone(), ServerSlot::new(ServerStatus::Running(backend)));
                None
            }
            Some(_) => Some(backend),
        }
    }

    /// Removes the backend of `id` for good, leaving the slot stopped, and
    /// returns its server.
    pub(super) fn remove_server(&mut self, id: &ServerId) -> Option<LspServer> {
        let slot = self.0.get_mut(id)?;
        if !matches!(slot.status, ServerStatus::Running(_)) {
            return None;
        }
        match std::mem::replace(
            &mut slot.status,
            ServerStatus::Expected {
                prior_failure: None,
            },
        ) {
            ServerStatus::Running(Backend::Process(server)) => {
                slot.status = ServerStatus::Stopped(server.client().clone());
                Some(server)
            }
            #[cfg(test)]
            ServerStatus::Running(Backend::Split { client, server }) => {
                slot.status = ServerStatus::Stopped(client);
                Some(server)
            }
            #[cfg(test)]
            ServerStatus::Running(backend @ Backend::ClientOnly(_)) => {
                slot.status = ServerStatus::Running(backend);
                None
            }
            other => {
                slot.status = other;
                None
            }
        }
    }

    /// Stops every running server that has one, for shutdown, returning them.
    pub(super) fn drain_servers(&mut self) -> Vec<(ServerId, LspServer)> {
        let ids: Vec<ServerId> = self.0.keys().cloned().collect();
        ids.into_iter()
            .filter_map(|id| self.remove_server(&id).map(|server| (id, server)))
            .collect()
    }

    /// The settlement of every slot, for re-deriving the routing table.
    pub(super) fn settlements(&self) -> HashMap<ServerId, crate::config::ServerSettlement> {
        use crate::config::ServerSettlement;

        self.0
            .iter()
            .map(|(id, slot)| {
                let settlement = match &slot.status {
                    ServerStatus::Running(_) | ServerStatus::Stopped(_) => {
                        ServerSettlement::Registered
                    }
                    ServerStatus::Failed(_)
                    | ServerStatus::Expected {
                        prior_failure: Some(_),
                    } => ServerSettlement::Failed,
                    ServerStatus::Expected {
                        prior_failure: None,
                    }
                    | ServerStatus::Restarting => ServerSettlement::Pending,
                };
                (id.clone(), settlement)
            })
            .collect()
    }

    /// The respawn lock of `id`, or a detached one for an unknown id (nobody
    /// else can be racing on a server that has no slot).
    pub(super) fn respawn_lock(&self, id: &ServerId) -> Arc<Mutex<()>> {
        self.0.get(id).map_or_else(
            || Arc::new(Mutex::new(())),
            |slot| Arc::clone(&slot.respawn_lock),
        )
    }

    /// The slot of `id`, created running with `backend` when absent.
    ///
    /// A stopped slot is never revived: the new backend is handed back as the
    /// `Err`, for the caller to drop outside the guard.
    pub(super) fn slot_for_swap(
        &mut self,
        id: &ServerId,
        backend: Backend,
    ) -> Result<Option<Backend>, Box<Backend>> {
        if let Some(slot) = self.0.get_mut(id) {
            if matches!(slot.status, ServerStatus::Stopped(_)) {
                return Err(Box::new(backend));
            }
            return Ok(
                match std::mem::replace(&mut slot.status, ServerStatus::Running(backend)) {
                    ServerStatus::Running(old) => Some(old),
                    _ => None,
                },
            );
        }
        self.0
            .insert(id.clone(), ServerSlot::new(ServerStatus::Running(backend)));
        Ok(None)
    }

    /// Test-only: registers a bare client, next to the server already there.
    #[cfg(test)]
    pub(super) fn register_test_client(&mut self, id: ServerId, client: LspClient) {
        let backend = match self.0.get_mut(&id).map(|slot| {
            std::mem::replace(
                &mut slot.status,
                ServerStatus::Expected {
                    prior_failure: None,
                },
            )
        }) {
            Some(ServerStatus::Running(
                Backend::Process(server) | Backend::Split { server, .. },
            )) => Backend::Split { client, server },
            _ => Backend::ClientOnly(client),
        };
        self.register(id, backend);
    }

    /// Test-only: registers a bare server, next to the client already there.
    #[cfg(test)]
    pub(super) fn register_test_server(&mut self, id: ServerId, server: LspServer) {
        let backend = match self.0.get_mut(&id).map(|slot| {
            std::mem::replace(
                &mut slot.status,
                ServerStatus::Expected {
                    prior_failure: None,
                },
            )
        }) {
            Some(ServerStatus::Running(
                Backend::ClientOnly(client) | Backend::Split { client, .. },
            )) => Backend::Split { client, server },
            _ => Backend::Process(server),
        };
        self.register(id, backend);
    }
}

/// What [`Servers::register`] found in the slot it overwrote.
#[derive(Debug)]
pub(super) struct Registered {
    /// The server was waiting for its startup to settle.
    pub(super) was_expected: bool,
    /// A running backend that the registration replaced.
    pub(super) displaced: Option<Backend>,
}

/// The translator-wide lifecycle, one value instead of three flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum Phase {
    /// Nothing started yet.
    #[default]
    Idle,
    /// The initial servers are still settling.
    Settling,
    /// Startup settled.
    Settled,
    /// The background init task panicked, so no notification wiring will come.
    InitPanicked,
    /// Shutdown has begun; terminal.
    ShuttingDown,
}

impl Phase {
    pub(super) const fn begin_startup(&mut self) {
        if matches!(self, Self::Idle | Self::Settled) {
            *self = Self::Settling;
        }
    }

    pub(super) fn finish_startup(&mut self) {
        if *self == Self::Settling {
            *self = Self::Settled;
        }
    }

    pub(super) fn init_panicked(&mut self) {
        if *self != Self::ShuttingDown {
            *self = Self::InitPanicked;
        }
    }

    pub(super) const fn begin_shutdown(&mut self) {
        *self = Self::ShuttingDown;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::error::StartupFailure;

    #[test]
    fn test_phase_shutdown_is_terminal_and_init_panic_survives_settling() {
        let mut phase = Phase::default();
        phase.begin_startup();
        phase.init_panicked();
        phase.finish_startup();
        assert_eq!(phase, Phase::InitPanicked);

        phase.begin_shutdown();
        phase.init_panicked();
        phase.begin_startup();
        assert_eq!(phase, Phase::ShuttingDown);
    }

    #[test]
    fn test_phase_startup_guard_drop_only_leaves_settling() {
        let mut phase = Phase::Idle;
        phase.finish_startup();
        assert_eq!(phase, Phase::Idle);
        phase.begin_startup();
        phase.finish_startup();
        assert_eq!(phase, Phase::Settled);
    }

    fn failure(id: &str) -> ServerSpawnFailure {
        ServerSpawnFailure {
            server_id: ServerId::from(id),
            language_id: "rust".to_string(),
            command: "x".to_string(),
            reason: StartupFailure::InitTaskPanicked,
        }
    }

    #[test]
    fn test_failed_then_expected_then_cleared_keeps_the_failure() {
        let mut servers = Servers::default();
        let id = ServerId::from("rust");
        servers.record_failure(&failure("rust"));
        servers.set_expected(&HashSet::from([id.clone()]));
        assert!(servers.is_expected(&id));
        assert!(servers.failure(&id).is_some());

        servers.clear_expected();

        assert!(!servers.is_expected(&id));
        assert!(servers.failure(&id).is_some());
    }

    #[test]
    fn test_failure_of_an_expected_server_settles_it_as_failed() {
        let mut servers = Servers::default();
        let id = ServerId::from("rust");
        servers.set_expected(&HashSet::from([id.clone()]));
        assert!(servers.record_failure(&failure("rust")));
        assert!(servers.failure(&id).is_some());
        assert!(!servers.is_expected(&id));
        assert_eq!(servers.failures().len(), 1);
    }

    fn running_backend() -> Backend {
        Backend::Process(LspServer::new_for_test(
            lsp_types::ServerCapabilities::default(),
        ))
    }

    #[tokio::test]
    async fn test_failure_on_a_running_or_stopped_slot_is_ignored() {
        let mut servers = Servers::default();
        let id = ServerId::from("rust");
        servers.register(id.clone(), running_backend());

        assert!(!servers.record_failure(&failure("rust")));
        assert!(servers.failure(&id).is_none());
        assert!(servers.server(&id).is_some());

        let stopped = servers.remove_server(&id);
        assert!(stopped.is_some());
        assert!(!servers.record_failure(&failure("rust")));
        assert!(servers.failure(&id).is_none());
    }

    #[tokio::test]
    async fn test_stopped_slot_keeps_the_dead_client_and_is_never_revived_by_a_swap() {
        let mut servers = Servers::default();
        let id = ServerId::from("rust");
        servers.register(id.clone(), running_backend());
        drop(servers.remove_server(&id));

        assert!(servers.client(&id).is_some());
        assert!(servers.server(&id).is_none());
        assert!(servers.slot_for_swap(&id, running_backend()).is_err());
        assert!(servers.server(&id).is_none());
    }

    #[tokio::test]
    async fn test_swap_replaces_the_backend_in_one_assignment_and_returns_the_old_one() {
        let mut servers = Servers::default();
        let id = ServerId::from("rust");
        servers.register(id.clone(), running_backend());

        let old = servers.slot_for_swap(&id, running_backend()).unwrap();

        assert!(old.is_some());
        let server = servers.server(&id).unwrap();
        assert!(std::ptr::eq(
            std::ptr::from_ref(server.client()),
            std::ptr::from_ref(server.client())
        ));
        assert!(servers.client(&id).is_some());
    }

    #[tokio::test]
    async fn test_take_and_restore_round_trip_and_refuse_a_settled_slot() {
        let mut servers = Servers::default();
        let id = ServerId::from("rust");
        servers.register(id.clone(), running_backend());

        let held = servers.take_for_restart(&id).unwrap();
        assert!(servers.is_expected(&id));
        assert!(servers.restore(&id, held).is_none());
        assert!(servers.server(&id).is_some());

        let held = servers.take_for_restart(&id).unwrap();
        servers.register(id.clone(), running_backend());
        assert!(servers.restore(&id, held).is_some());
    }

    #[test]
    fn test_clear_expected_keeps_failed_and_drops_plain_expected() {
        let mut servers = Servers::default();
        let (failed, plain) = (ServerId::from("failed"), ServerId::from("plain"));
        servers.record_failure(&failure("failed"));
        servers.set_expected(&HashSet::from([failed.clone(), plain.clone()]));

        servers.clear_expected();

        assert!(servers.failure(&failed).is_some());
        assert!(servers.get(&plain).is_none());
    }

    #[test]
    fn test_set_expected_drops_expectation_of_servers_outside_the_set() {
        let mut servers = Servers::default();
        let (a, b) = (ServerId::from("a"), ServerId::from("b"));
        servers.set_expected(&HashSet::from([a.clone(), b.clone()]));
        servers.set_expected(&HashSet::from([a.clone()]));
        assert!(servers.is_expected(&a));
        assert!(!servers.is_expected(&b));
        assert!(servers.get(&b).is_none());
    }
}
