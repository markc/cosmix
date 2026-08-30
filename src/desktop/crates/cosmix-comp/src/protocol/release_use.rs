use std::{
    collections::{HashMap, HashSet},
    fmt,
    hash::Hash,
    mem,
};

pub(super) const MAX_CLIENT_DMABUF_USES: usize = 256;
pub(super) const MAX_GLOBAL_DMABUF_USES: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct DmabufUseId(pub(super) u64);

impl DmabufUseId {
    #[cfg(test)]
    pub(crate) fn for_test(raw: u64) -> Self {
        Self(raw)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ReleaseUseFailure {
    ExplicitSyncFaulted,
    ClientBudget { count: usize, limit: usize },
    GlobalBudget { count: usize, limit: usize },
    DuplicateInitialToken { token: u64 },
    TokenAlreadyOwned { token: u64 },
}

impl fmt::Display for ReleaseUseFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExplicitSyncFaulted => {
                formatter.write_str("explicit-sync retirement is permanently faulted")
            }
            Self::ClientBudget { count, limit } => {
                write!(
                    formatter,
                    "client DMA-BUF use budget exceeded ({count}/{limit})"
                )
            }
            Self::GlobalBudget { count, limit } => {
                write!(
                    formatter,
                    "global DMA-BUF use budget exceeded ({count}/{limit})"
                )
            }
            Self::DuplicateInitialToken { token } => {
                write!(
                    formatter,
                    "DMA-BUF use received duplicate owner token {token}"
                )
            }
            Self::TokenAlreadyOwned { token } => {
                write!(
                    formatter,
                    "DMA-BUF owner token {token} already belongs to a use"
                )
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BeginUseDecision {
    Implicit,
    Begun(DmabufUseId),
    Rejected(ReleaseUseFailure),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AddRendererOwnerDecision {
    Implicit,
    Added,
    UnknownUse,
    TokenAlreadyOwned,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ReleaseOwnerDecision {
    Released,
    Retired(DmabufUseId),
    Faulted(DmabufUseId),
    UnknownToken,
}

pub(super) struct RetiredUse<ClientKey, BufferKey, Point> {
    pub(super) id: DmabufUseId,
    pub(super) client: ClientKey,
    pub(super) buffer: BufferKey,
    pub(super) release_point: Point,
}

pub(super) struct AbandonedUse<ClientKey, BufferKey, Point> {
    pub(super) id: DmabufUseId,
    pub(super) client: ClientKey,
    pub(super) buffer: BufferKey,
    pub(super) release_point: Point,
}

pub(super) struct TerminalUse<ClientKey> {
    pub(super) id: DmabufUseId,
    pub(super) client: ClientKey,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ReleaseUseAbandonReason {
    OrderlyShutdown,
    DispatchFailure,
    ServerDrop,
    ExplicitSyncFault,
}

pub(super) enum RetirementUpdate<ClientKey> {
    Awaiting,
    Completed(Vec<TerminalUse<ClientKey>>),
    Faulted(Vec<TerminalUse<ClientKey>>),
}

pub(super) trait ReleaseUsePlatform {
    type ClientKey: Clone + Eq + Hash;
    type Client;
    type BufferKey: Clone;
    type Point;
    type RetirementEvent;

    fn explicit_sync_healthy(&self) -> bool;

    fn retire_use(
        &mut self,
        retired: RetiredUse<Self::ClientKey, Self::BufferKey, Self::Point>,
    ) -> RetirementUpdate<Self::ClientKey>;
    fn handle_retirement_event(
        &mut self,
        event: Self::RetirementEvent,
    ) -> RetirementUpdate<Self::ClientKey>;
    fn retirement_worker_closed(&mut self) -> RetirementUpdate<Self::ClientKey>;
    fn stop_retirement_worker(&mut self);
    fn disconnect_explicit_sync_client(&mut self, client: &Self::ClientKey);
    fn abandon_use(
        &mut self,
        abandoned: AbandonedUse<Self::ClientKey, Self::BufferKey, Self::Point>,
        reason: ReleaseUseAbandonReason,
    );
    fn abandon_retired_uses(
        &mut self,
        reason: ReleaseUseAbandonReason,
    ) -> Vec<TerminalUse<Self::ClientKey>>;
    fn reject_use(
        &mut self,
        client: &Self::Client,
        release_point: Self::Point,
        failure: &ReleaseUseFailure,
    );
}

struct LiveUse<P: ReleaseUsePlatform> {
    client: P::ClientKey,
    buffer: P::BufferKey,
    release_point: P::Point,
    backing_owner: Option<u64>,
    renderer_owners: HashSet<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OwnerKind {
    Backing,
    Renderer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TokenOwner {
    use_id: DmabufUseId,
    kind: OwnerKind,
}

pub(super) struct ReleaseUseEngine<P: ReleaseUsePlatform> {
    platform: P,
    next_use_id: u64,
    live: HashMap<DmabufUseId, LiveUse<P>>,
    token_owners: HashMap<u64, TokenOwner>,
    owned_uses: HashMap<DmabufUseId, P::ClientKey>,
    uses_by_client: HashMap<P::ClientKey, usize>,
    #[cfg(test)]
    forced_failure: Option<ReleaseUseFailure>,
}

impl<P: ReleaseUsePlatform> ReleaseUseEngine<P> {
    pub(super) fn new(platform: P) -> Self {
        Self {
            platform,
            next_use_id: 1,
            live: HashMap::new(),
            token_owners: HashMap::new(),
            owned_uses: HashMap::new(),
            uses_by_client: HashMap::new(),
            #[cfg(test)]
            forced_failure: None,
        }
    }

    pub(super) fn prepare_use(
        &mut self,
        client: P::ClientKey,
        client_handle: &P::Client,
        buffer: P::BufferKey,
        release_point: Option<P::Point>,
        backing_token: u64,
        renderer_token: u64,
    ) -> BeginUseDecision {
        let Some(release_point) = release_point else {
            return BeginUseDecision::Implicit;
        };
        if !self.platform.explicit_sync_healthy() {
            let failure = ReleaseUseFailure::ExplicitSyncFaulted;
            self.platform
                .reject_use(client_handle, release_point, &failure);
            return BeginUseDecision::Rejected(failure);
        }
        #[cfg(test)]
        if let Some(failure) = self.forced_failure.take() {
            self.platform
                .reject_use(client_handle, release_point, &failure);
            return BeginUseDecision::Rejected(failure);
        }
        if backing_token == renderer_token {
            let failure = ReleaseUseFailure::DuplicateInitialToken {
                token: backing_token,
            };
            self.platform
                .reject_use(client_handle, release_point, &failure);
            return BeginUseDecision::Rejected(failure);
        }
        if self.token_owners.contains_key(&backing_token) {
            let failure = ReleaseUseFailure::TokenAlreadyOwned {
                token: backing_token,
            };
            self.platform
                .reject_use(client_handle, release_point, &failure);
            return BeginUseDecision::Rejected(failure);
        }
        if self.token_owners.contains_key(&renderer_token) {
            let failure = ReleaseUseFailure::TokenAlreadyOwned {
                token: renderer_token,
            };
            self.platform
                .reject_use(client_handle, release_point, &failure);
            return BeginUseDecision::Rejected(failure);
        }

        let client_count = self
            .uses_by_client
            .get(&client)
            .copied()
            .unwrap_or_default();
        if client_count >= MAX_CLIENT_DMABUF_USES {
            let failure = ReleaseUseFailure::ClientBudget {
                count: client_count,
                limit: MAX_CLIENT_DMABUF_USES,
            };
            self.platform
                .reject_use(client_handle, release_point, &failure);
            return BeginUseDecision::Rejected(failure);
        }
        let global_count = self.owned_uses.len();
        if global_count >= MAX_GLOBAL_DMABUF_USES {
            let failure = ReleaseUseFailure::GlobalBudget {
                count: global_count,
                limit: MAX_GLOBAL_DMABUF_USES,
            };
            self.platform
                .reject_use(client_handle, release_point, &failure);
            return BeginUseDecision::Rejected(failure);
        }

        let use_id = self.allocate_use_id();
        self.live.insert(
            use_id,
            LiveUse {
                client: client.clone(),
                buffer,
                release_point,
                backing_owner: Some(backing_token),
                renderer_owners: HashSet::from([renderer_token]),
            },
        );
        self.token_owners.insert(
            backing_token,
            TokenOwner {
                use_id,
                kind: OwnerKind::Backing,
            },
        );
        self.token_owners.insert(
            renderer_token,
            TokenOwner {
                use_id,
                kind: OwnerKind::Renderer,
            },
        );
        assert!(
            self.owned_uses.insert(use_id, client.clone()).is_none(),
            "new DMA-BUF use ID must not already own a release point"
        );
        self.uses_by_client.insert(client, client_count + 1);
        BeginUseDecision::Begun(use_id)
    }

    pub(super) fn add_renderer_owner(
        &mut self,
        use_id: Option<DmabufUseId>,
        token: u64,
    ) -> AddRendererOwnerDecision {
        let Some(use_id) = use_id else {
            return AddRendererOwnerDecision::Implicit;
        };
        if self.token_owners.contains_key(&token) {
            return AddRendererOwnerDecision::TokenAlreadyOwned;
        }
        let Some(live) = self.live.get_mut(&use_id) else {
            return AddRendererOwnerDecision::UnknownUse;
        };
        live.renderer_owners.insert(token);
        self.token_owners.insert(
            token,
            TokenOwner {
                use_id,
                kind: OwnerKind::Renderer,
            },
        );
        AddRendererOwnerDecision::Added
    }

    pub(super) fn release_owner(&mut self, token: u64) -> ReleaseOwnerDecision {
        let Some(owner) = self.token_owners.remove(&token) else {
            return ReleaseOwnerDecision::UnknownToken;
        };
        let live = self
            .live
            .get_mut(&owner.use_id)
            .expect("token owner references a live DMA-BUF use");
        match owner.kind {
            OwnerKind::Backing => {
                let removed = live.backing_owner.take();
                assert_eq!(removed, Some(token), "backing owner token must match use");
            }
            OwnerKind::Renderer => {
                assert!(
                    live.renderer_owners.remove(&token),
                    "renderer owner token must belong to use"
                );
            }
        }
        if live.backing_owner.is_some() || !live.renderer_owners.is_empty() {
            return ReleaseOwnerDecision::Released;
        }

        let live = self
            .live
            .remove(&owner.use_id)
            .expect("terminal DMA-BUF use remains live");
        let update = self.platform.retire_use(RetiredUse {
            id: owner.use_id,
            client: live.client,
            buffer: live.buffer,
            release_point: live.release_point,
        });
        if self.apply_retirement_update(update) {
            ReleaseOwnerDecision::Faulted(owner.use_id)
        } else {
            ReleaseOwnerDecision::Retired(owner.use_id)
        }
    }

    pub(super) fn handle_retirement_event(&mut self, event: P::RetirementEvent) -> bool {
        let update = self.platform.handle_retirement_event(event);
        self.apply_retirement_update(update)
    }

    pub(super) fn retirement_worker_closed(&mut self) -> bool {
        let update = self.platform.retirement_worker_closed();
        self.apply_retirement_update(update)
    }

    pub(super) fn stop_retirement_worker(&mut self) {
        self.platform.stop_retirement_worker();
    }

    pub(super) fn abandon_all(&mut self, reason: ReleaseUseAbandonReason) -> usize {
        let live = mem::take(&mut self.live);
        self.token_owners.clear();
        let live_count = live.len();
        for (id, live) in live {
            let accounting_client = live.client.clone();
            self.platform.abandon_use(
                AbandonedUse {
                    id,
                    client: live.client,
                    buffer: live.buffer,
                    release_point: live.release_point,
                },
                reason,
            );
            self.release_terminal_use(TerminalUse {
                id,
                client: accounting_client,
            });
        }
        let retired = self.platform.abandon_retired_uses(reason);
        let retired_count = retired.len();
        for terminal in retired {
            self.release_terminal_use(terminal);
        }
        assert!(
            self.owned_uses.is_empty() && self.uses_by_client.is_empty(),
            "abandoning all DMA-BUF release points clears ownership accounting"
        );
        live_count + retired_count
    }

    #[cfg(test)]
    pub(super) fn force_next_failure(&mut self, failure: ReleaseUseFailure) {
        self.forced_failure = Some(failure);
    }

    #[cfg(test)]
    pub(super) fn owner_kind_name(&self, token: u64) -> Option<&'static str> {
        self.token_owners.get(&token).map(|owner| match owner.kind {
            OwnerKind::Backing => "backing",
            OwnerKind::Renderer => "renderer",
        })
    }

    #[cfg(test)]
    pub(super) fn accounting_counts(&self, client: &P::ClientKey) -> (usize, usize) {
        (
            self.uses_by_client.get(client).copied().unwrap_or_default(),
            self.owned_uses.len(),
        )
    }

    #[cfg(test)]
    pub(super) fn explicit_sync_healthy(&self) -> bool {
        self.platform.explicit_sync_healthy()
    }

    #[cfg(test)]
    pub(super) fn platform_mut(&mut self) -> &mut P {
        &mut self.platform
    }

    fn allocate_use_id(&mut self) -> DmabufUseId {
        loop {
            let candidate = DmabufUseId(self.next_use_id);
            self.next_use_id = self.next_use_id.wrapping_add(1);
            if !self.owned_uses.contains_key(&candidate) {
                return candidate;
            }
        }
    }

    pub(super) fn release_terminal_use(&mut self, terminal: TerminalUse<P::ClientKey>) {
        let accounted_client = self
            .owned_uses
            .remove(&terminal.id)
            .expect("terminal DMA-BUF use has ownership accounting");
        assert!(
            accounted_client == terminal.client,
            "terminal DMA-BUF use client must match ownership accounting"
        );
        self.release_client_use(&terminal.client);
    }

    fn apply_retirement_update(&mut self, update: RetirementUpdate<P::ClientKey>) -> bool {
        match update {
            RetirementUpdate::Awaiting => false,
            RetirementUpdate::Completed(terminal) => {
                for terminal in terminal {
                    self.release_terminal_use(terminal);
                }
                false
            }
            RetirementUpdate::Faulted(terminal) => {
                let clients = self.owned_uses.values().cloned().collect::<HashSet<_>>();
                for client in clients {
                    self.platform.disconnect_explicit_sync_client(&client);
                }
                for terminal in terminal {
                    self.release_terminal_use(terminal);
                }
                self.abandon_all(ReleaseUseAbandonReason::ExplicitSyncFault);
                true
            }
        }
    }

    fn release_client_use(&mut self, client: &P::ClientKey) {
        let count = self
            .uses_by_client
            .get_mut(client)
            .expect("live DMA-BUF use has client accounting");
        *count = count
            .checked_sub(1)
            .expect("DMA-BUF use client accounting underflow");
        if *count == 0 {
            self.uses_by_client.remove(client);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakePoint(u64);

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct Retired {
        id: DmabufUseId,
        client: u32,
        buffer: u32,
        point: FakePoint,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct Rejected {
        client: u32,
        point: FakePoint,
        failure: ReleaseUseFailure,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct Abandoned {
        id: DmabufUseId,
        client: u32,
        buffer: u32,
        point: FakePoint,
        reason: ReleaseUseAbandonReason,
    }

    #[derive(Default)]
    struct FakePlatform {
        retired: Vec<Retired>,
        rejected: Vec<Rejected>,
        abandoned: Vec<Abandoned>,
    }

    impl ReleaseUsePlatform for FakePlatform {
        type ClientKey = u32;
        type Client = u32;
        type BufferKey = u32;
        type Point = FakePoint;
        type RetirementEvent = ();

        fn explicit_sync_healthy(&self) -> bool {
            true
        }

        fn retire_use(
            &mut self,
            retired: RetiredUse<Self::ClientKey, Self::BufferKey, Self::Point>,
        ) -> RetirementUpdate<Self::ClientKey> {
            self.retired.push(Retired {
                id: retired.id,
                client: retired.client,
                buffer: retired.buffer,
                point: retired.release_point,
            });
            RetirementUpdate::Awaiting
        }

        fn handle_retirement_event(
            &mut self,
            _event: Self::RetirementEvent,
        ) -> RetirementUpdate<Self::ClientKey> {
            RetirementUpdate::Awaiting
        }

        fn retirement_worker_closed(&mut self) -> RetirementUpdate<Self::ClientKey> {
            RetirementUpdate::Awaiting
        }

        fn stop_retirement_worker(&mut self) {}

        fn disconnect_explicit_sync_client(&mut self, _client: &Self::ClientKey) {}

        fn abandon_use(
            &mut self,
            abandoned: AbandonedUse<Self::ClientKey, Self::BufferKey, Self::Point>,
            reason: ReleaseUseAbandonReason,
        ) {
            self.abandoned.push(Abandoned {
                id: abandoned.id,
                client: abandoned.client,
                buffer: abandoned.buffer,
                point: abandoned.release_point,
                reason,
            });
        }

        fn abandon_retired_uses(
            &mut self,
            reason: ReleaseUseAbandonReason,
        ) -> Vec<TerminalUse<Self::ClientKey>> {
            let retired = mem::take(&mut self.retired);
            let terminal = retired
                .iter()
                .map(|retired| TerminalUse {
                    id: retired.id,
                    client: retired.client,
                })
                .collect();
            self.abandoned
                .extend(retired.into_iter().map(|retired| Abandoned {
                    id: retired.id,
                    client: retired.client,
                    buffer: retired.buffer,
                    point: retired.point,
                    reason,
                }));
            terminal
        }

        fn reject_use(
            &mut self,
            client: &Self::Client,
            release_point: Self::Point,
            failure: &ReleaseUseFailure,
        ) {
            self.rejected.push(Rejected {
                client: *client,
                point: release_point,
                failure: *failure,
            });
        }
    }

    fn engine() -> ReleaseUseEngine<FakePlatform> {
        ReleaseUseEngine::new(FakePlatform::default())
    }

    fn begin(
        engine: &mut ReleaseUseEngine<FakePlatform>,
        client: u32,
        buffer: u32,
        point: u64,
        backing_token: u64,
        renderer_token: u64,
    ) -> DmabufUseId {
        let BeginUseDecision::Begun(use_id) = engine.prepare_use(
            client,
            &client,
            buffer,
            Some(FakePoint(point)),
            backing_token,
            renderer_token,
        ) else {
            panic!("fake DMA-BUF use must begin");
        };
        use_id
    }

    #[test]
    fn use_limits_match_the_existing_retained_dmabuf_budgets() {
        assert_eq!(
            MAX_CLIENT_DMABUF_USES,
            super::super::MAX_CLIENT_RETAINED_DMABUFS
        );
        assert_eq!(
            MAX_GLOBAL_DMABUF_USES,
            super::super::MAX_GLOBAL_RETAINED_DMABUFS
        );
    }

    #[test]
    fn implicit_commit_does_not_reserve_or_call_the_release_platform() {
        let mut engine = engine();

        let decision = engine.prepare_use(1, &1, 77, None, 1, 2);

        assert_eq!(decision, BeginUseDecision::Implicit);
        assert!(engine.live.is_empty());
        assert!(engine.token_owners.is_empty());
        assert!(engine.owned_uses.is_empty());
        assert!(engine.uses_by_client.is_empty());
        assert!(engine.platform.retired.is_empty());
        assert!(engine.platform.rejected.is_empty());
    }

    #[test]
    fn implicit_dirty_recovery_token_does_not_join_an_explicit_owner_map() {
        let mut engine = engine();

        let decision = engine.add_renderer_owner(None, 44);

        assert_eq!(decision, AddRendererOwnerDecision::Implicit);
        assert!(engine.live.is_empty());
        assert!(engine.token_owners.is_empty());
        assert_eq!(engine.release_owner(44), ReleaseOwnerDecision::UnknownToken);
    }

    #[test]
    fn same_buffer_committed_twice_has_two_independent_uses() {
        let mut engine = engine();
        let first = begin(&mut engine, 1, 77, 101, 1, 2);
        let second = begin(&mut engine, 1, 77, 102, 3, 4);

        assert_ne!(first, second);
        assert_eq!(engine.release_owner(1), ReleaseOwnerDecision::Released);
        assert_eq!(
            engine.release_owner(2),
            ReleaseOwnerDecision::Retired(first)
        );
        assert_eq!(engine.platform.retired.len(), 1);
        assert_eq!(engine.platform.retired[0].buffer, 77);
        assert_eq!(engine.platform.retired[0].point, FakePoint(101));
        assert!(engine.live.contains_key(&second));
        assert_eq!(engine.release_owner(3), ReleaseOwnerDecision::Released);
        assert_eq!(
            engine.release_owner(4),
            ReleaseOwnerDecision::Retired(second)
        );
        assert_eq!(engine.platform.retired[1].point, FakePoint(102));
    }

    #[test]
    fn surface_destruction_releases_backing_but_retains_renderer_ownership() {
        let mut engine = engine();
        let use_id = begin(&mut engine, 1, 8, 9, 10, 11);

        assert_eq!(engine.release_owner(10), ReleaseOwnerDecision::Released);
        assert!(engine.platform.retired.is_empty());
        assert_eq!(
            engine.release_owner(11),
            ReleaseOwnerDecision::Retired(use_id)
        );
    }

    #[test]
    fn renderer_first_waits_for_backing_owner() {
        let mut engine = engine();
        let use_id = begin(&mut engine, 1, 8, 9, 10, 11);

        assert_eq!(engine.release_owner(11), ReleaseOwnerDecision::Released);
        assert!(engine.platform.retired.is_empty());
        assert_eq!(
            engine.release_owner(10),
            ReleaseOwnerDecision::Retired(use_id)
        );
        assert_eq!(engine.uses_by_client.get(&1), Some(&1));
        assert_eq!(engine.owned_uses.get(&use_id), Some(&1));
    }

    #[test]
    fn two_renderer_tokens_must_both_be_released() {
        let mut engine = engine();
        let use_id = begin(&mut engine, 1, 8, 9, 10, 11);
        assert_eq!(
            engine.add_renderer_owner(Some(use_id), 12),
            AddRendererOwnerDecision::Added
        );

        assert_eq!(engine.release_owner(10), ReleaseOwnerDecision::Released);
        assert_eq!(engine.release_owner(11), ReleaseOwnerDecision::Released);
        assert!(engine.platform.retired.is_empty());
        assert_eq!(
            engine.release_owner(12),
            ReleaseOwnerDecision::Retired(use_id)
        );
    }

    #[test]
    fn outbox_eviction_then_dirty_recovery_rejoins_the_same_use() {
        let mut engine = engine();
        let use_id = begin(&mut engine, 1, 8, 9, 10, 11);

        assert_eq!(engine.release_owner(11), ReleaseOwnerDecision::Released);
        assert_eq!(
            engine.add_renderer_owner(Some(use_id), 12),
            AddRendererOwnerDecision::Added
        );
        assert_eq!(engine.release_owner(10), ReleaseOwnerDecision::Released);
        assert!(engine.platform.retired.is_empty());
        assert_eq!(
            engine.release_owner(12),
            ReleaseOwnerDecision::Retired(use_id)
        );
    }

    #[test]
    fn duplicate_and_unknown_token_releases_do_not_change_another_owner() {
        let mut engine = engine();
        let use_id = begin(&mut engine, 1, 8, 9, 10, 11);

        assert_eq!(
            engine.release_owner(999),
            ReleaseOwnerDecision::UnknownToken
        );
        assert_eq!(engine.release_owner(11), ReleaseOwnerDecision::Released);
        assert_eq!(engine.release_owner(11), ReleaseOwnerDecision::UnknownToken);
        assert!(engine.platform.retired.is_empty());
        assert_eq!(
            engine.release_owner(10),
            ReleaseOwnerDecision::Retired(use_id)
        );
    }

    #[test]
    fn abandon_all_clears_live_and_retired_uses_and_every_owner_token() {
        let mut engine = engine();
        let retired = begin(&mut engine, 1, 8, 9, 10, 11);
        let live = begin(&mut engine, 2, 18, 19, 20, 21);
        assert_eq!(engine.release_owner(10), ReleaseOwnerDecision::Released);
        assert_eq!(
            engine.release_owner(11),
            ReleaseOwnerDecision::Retired(retired)
        );

        assert_eq!(
            engine.abandon_all(ReleaseUseAbandonReason::OrderlyShutdown),
            2
        );

        assert!(engine.live.is_empty());
        assert!(engine.token_owners.is_empty());
        assert!(engine.owned_uses.is_empty());
        assert!(engine.uses_by_client.is_empty());
        assert!(engine.platform.retired.is_empty());
        assert_eq!(
            engine.platform.abandoned,
            [
                Abandoned {
                    id: live,
                    client: 2,
                    buffer: 18,
                    point: FakePoint(19),
                    reason: ReleaseUseAbandonReason::OrderlyShutdown,
                },
                Abandoned {
                    id: retired,
                    client: 1,
                    buffer: 8,
                    point: FakePoint(9),
                    reason: ReleaseUseAbandonReason::OrderlyShutdown,
                },
            ]
        );
        assert_eq!(engine.release_owner(20), ReleaseOwnerDecision::UnknownToken);
        assert_eq!(engine.release_owner(21), ReleaseOwnerDecision::UnknownToken);
        assert_eq!(
            engine.abandon_all(ReleaseUseAbandonReason::OrderlyShutdown),
            0
        );
    }

    #[test]
    fn renderer_owner_rejects_unknown_use_without_claiming_token() {
        let mut engine = engine();

        assert_eq!(
            engine.add_renderer_owner(Some(DmabufUseId(88)), 12),
            AddRendererOwnerDecision::UnknownUse
        );
        let use_id = begin(&mut engine, 1, 8, 9, 10, 12);
        assert_eq!(engine.release_owner(10), ReleaseOwnerDecision::Released);
        assert_eq!(
            engine.release_owner(12),
            ReleaseOwnerDecision::Retired(use_id)
        );
    }

    #[test]
    fn renderer_owner_rejects_a_token_already_owned_by_another_use() {
        let mut engine = engine();
        let first = begin(&mut engine, 1, 8, 9, 10, 11);
        let second = begin(&mut engine, 1, 9, 10, 20, 21);

        assert_eq!(
            engine.add_renderer_owner(Some(second), 11),
            AddRendererOwnerDecision::TokenAlreadyOwned
        );
        assert_eq!(engine.release_owner(10), ReleaseOwnerDecision::Released);
        assert_eq!(
            engine.release_owner(11),
            ReleaseOwnerDecision::Retired(first)
        );
        assert_eq!(engine.release_owner(20), ReleaseOwnerDecision::Released);
        assert_eq!(
            engine.release_owner(21),
            ReleaseOwnerDecision::Retired(second)
        );
    }

    #[test]
    fn duplicate_initial_owner_tokens_are_rejected_and_point_is_returned_to_platform() {
        let mut engine = engine();

        let decision = engine.prepare_use(1, &1, 8, Some(FakePoint(9)), 10, 10);

        assert_eq!(
            decision,
            BeginUseDecision::Rejected(ReleaseUseFailure::DuplicateInitialToken { token: 10 })
        );
        assert!(engine.live.is_empty());
        assert!(engine.token_owners.is_empty());
        assert_eq!(engine.platform.rejected.len(), 1);
        assert_eq!(engine.platform.rejected[0].point, FakePoint(9));
    }

    #[test]
    fn initially_owned_token_is_rejected_without_disturbing_first_use() {
        let mut engine = engine();
        let first = begin(&mut engine, 1, 8, 9, 10, 11);

        let decision = engine.prepare_use(2, &2, 9, Some(FakePoint(10)), 20, 11);

        assert_eq!(
            decision,
            BeginUseDecision::Rejected(ReleaseUseFailure::TokenAlreadyOwned { token: 11 })
        );
        assert_eq!(engine.platform.rejected[0].client, 2);
        assert_eq!(engine.release_owner(10), ReleaseOwnerDecision::Released);
        assert_eq!(
            engine.release_owner(11),
            ReleaseOwnerDecision::Retired(first)
        );
    }

    #[test]
    fn initially_owned_backing_token_is_rejected_without_claiming_renderer_token() {
        let mut engine = engine();
        let first = begin(&mut engine, 1, 8, 9, 10, 11);

        let decision = engine.prepare_use(2, &2, 9, Some(FakePoint(10)), 10, 21);

        assert_eq!(
            decision,
            BeginUseDecision::Rejected(ReleaseUseFailure::TokenAlreadyOwned { token: 10 })
        );
        assert_eq!(engine.release_owner(21), ReleaseOwnerDecision::UnknownToken);
        assert_eq!(engine.release_owner(10), ReleaseOwnerDecision::Released);
        assert_eq!(
            engine.release_owner(11),
            ReleaseOwnerDecision::Retired(first)
        );
    }

    #[test]
    fn use_id_allocator_skips_a_live_id_after_wrap_or_collision() {
        let mut engine = engine();
        let first = begin(&mut engine, 1, 8, 9, 10, 11);
        engine.next_use_id = first.0;

        let second = begin(&mut engine, 2, 9, 10, 20, 21);

        assert_ne!(second, first);
        assert_eq!(second.0, first.0.wrapping_add(1));
    }

    #[test]
    fn use_id_allocator_wraps_without_panicking_or_reusing_the_last_id() {
        let mut engine = engine();
        engine.next_use_id = u64::MAX;

        let last = begin(&mut engine, 1, 8, 9, 10, 11);
        let wrapped = begin(&mut engine, 2, 9, 10, 20, 21);

        assert_eq!(last.0, u64::MAX);
        assert_eq!(wrapped.0, 0);
        assert_ne!(last, wrapped);
    }

    #[test]
    fn client_budget_accepts_limit_then_rejects_next_use() {
        let mut engine = engine();
        for index in 0..MAX_CLIENT_DMABUF_USES {
            let token = (index as u64) * 2 + 1;
            let _ = begin(&mut engine, 1, index as u32, token, token, token + 1);
        }

        let decision = engine.prepare_use(1, &1, 999, Some(FakePoint(999)), 10_000, 10_001);

        assert_eq!(
            decision,
            BeginUseDecision::Rejected(ReleaseUseFailure::ClientBudget {
                count: MAX_CLIENT_DMABUF_USES,
                limit: MAX_CLIENT_DMABUF_USES,
            })
        );
        assert_eq!(engine.live.len(), MAX_CLIENT_DMABUF_USES);
        assert_eq!(engine.platform.rejected.len(), 1);
    }

    #[test]
    fn retired_use_keeps_client_budget_until_terminal_abandonment() {
        let mut engine = engine();
        let first = begin(&mut engine, 1, 1, 1, 1, 2);
        for index in 1..MAX_CLIENT_DMABUF_USES {
            let token = (index as u64) * 2 + 1;
            let _ = begin(&mut engine, 1, index as u32, token, token, token + 1);
        }
        assert_eq!(engine.release_owner(1), ReleaseOwnerDecision::Released);
        assert_eq!(
            engine.release_owner(2),
            ReleaseOwnerDecision::Retired(first)
        );

        let replacement = engine.prepare_use(1, &1, 999, Some(FakePoint(999)), 10_000, 10_001);

        assert_eq!(
            replacement,
            BeginUseDecision::Rejected(ReleaseUseFailure::ClientBudget {
                count: MAX_CLIENT_DMABUF_USES,
                limit: MAX_CLIENT_DMABUF_USES,
            })
        );
        assert_eq!(engine.platform.retired.len(), 1);
        assert_eq!(engine.owned_uses.len(), MAX_CLIENT_DMABUF_USES);

        assert_eq!(
            engine.abandon_all(ReleaseUseAbandonReason::OrderlyShutdown),
            MAX_CLIENT_DMABUF_USES
        );
        assert!(matches!(
            engine.prepare_use(1, &1, 999, Some(FakePoint(1_000)), 10_002, 10_003),
            BeginUseDecision::Begun(_)
        ));
    }

    #[test]
    fn global_budget_accepts_limit_then_rejects_next_client() {
        let mut engine = engine();
        for index in 0..MAX_GLOBAL_DMABUF_USES {
            let token = (index as u64) * 2 + 1;
            let client = (index / (MAX_CLIENT_DMABUF_USES - 1)) as u32;
            let _ = begin(&mut engine, client, index as u32, token, token, token + 1);
        }

        let decision = engine.prepare_use(99, &99, 999, Some(FakePoint(999)), 10_000, 10_001);

        assert_eq!(
            decision,
            BeginUseDecision::Rejected(ReleaseUseFailure::GlobalBudget {
                count: MAX_GLOBAL_DMABUF_USES,
                limit: MAX_GLOBAL_DMABUF_USES,
            })
        );
        assert_eq!(engine.live.len(), MAX_GLOBAL_DMABUF_USES);
        assert_eq!(engine.platform.rejected.len(), 1);
    }

    #[test]
    fn retired_uses_keep_global_budget_until_terminal_abandonment() {
        let mut engine = engine();
        for index in 0..MAX_GLOBAL_DMABUF_USES {
            let token = (index as u64) * 2 + 1;
            let client = (index / (MAX_CLIENT_DMABUF_USES - 1)) as u32;
            let use_id = begin(&mut engine, client, index as u32, token, token, token + 1);
            assert_eq!(engine.release_owner(token), ReleaseOwnerDecision::Released);
            assert_eq!(
                engine.release_owner(token + 1),
                ReleaseOwnerDecision::Retired(use_id)
            );
        }

        assert!(engine.live.is_empty());
        assert_eq!(engine.platform.retired.len(), MAX_GLOBAL_DMABUF_USES);
        assert_eq!(engine.owned_uses.len(), MAX_GLOBAL_DMABUF_USES);
        assert_eq!(
            engine.prepare_use(99, &99, 999, Some(FakePoint(999)), 10_000, 10_001),
            BeginUseDecision::Rejected(ReleaseUseFailure::GlobalBudget {
                count: MAX_GLOBAL_DMABUF_USES,
                limit: MAX_GLOBAL_DMABUF_USES,
            })
        );

        assert_eq!(
            engine.abandon_all(ReleaseUseAbandonReason::ServerDrop),
            MAX_GLOBAL_DMABUF_USES
        );
        assert!(matches!(
            engine.prepare_use(99, &99, 999, Some(FakePoint(1_000)), 10_002, 10_003),
            BeginUseDecision::Begun(_)
        ));
    }
}
