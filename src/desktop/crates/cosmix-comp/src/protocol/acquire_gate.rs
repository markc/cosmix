use std::{
    collections::HashMap,
    fmt,
    hash::Hash,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
};

use smithay::{
    reexports::{
        calloop::{LoopHandle, RegistrationToken},
        wayland_server::{
            Client, DisplayHandle, backend::ClientId, protocol::wl_surface::WlSurface,
        },
    },
    wayland::{
        compositor::{self, Blocker, BlockerState},
        drm_syncobj::{DrmSyncPoint, DrmSyncPointBlocker, DrmSyncPointSource},
    },
};

use super::{WaylandState, terminate_resource_exhausting_client};

pub(super) const MAX_CLIENT_ACQUIRE_GATES: usize = 256;
pub(super) const MAX_GLOBAL_ACQUIRE_GATES: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(super) struct GateId(u64);

#[derive(Debug, Eq, PartialEq)]
pub(super) enum AcquireGateFailure {
    ClientBudget { count: usize, limit: usize },
    GlobalBudget { count: usize, limit: usize },
    Generate(String),
    Register(String),
}

impl fmt::Display for AcquireGateFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClientBudget { count, limit } => {
                write!(
                    formatter,
                    "client acquire-gate budget exceeded ({count}/{limit})"
                )
            }
            Self::GlobalBudget { count, limit } => {
                write!(
                    formatter,
                    "global acquire-gate budget exceeded ({count}/{limit})"
                )
            }
            Self::Generate(error) => {
                write!(formatter, "failed to generate acquire gate: {error}")
            }
            Self::Register(error) => {
                write!(formatter, "failed to register acquire-gate source: {error}")
            }
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum AcquireGateDecision {
    NoAcquirePoint,
    Armed(GateId),
    Rejected(AcquireGateFailure),
}

pub(super) struct PreparedGate<Source, Blocker, ReleaseHandle, CancelHandle> {
    source: Source,
    blocker: Blocker,
    release_handle: ReleaseHandle,
    cancel_handle: CancelHandle,
}

type PrepareResult<Source, Blocker, ReleaseHandle, CancelHandle> =
    Result<PreparedGate<Source, Blocker, ReleaseHandle, CancelHandle>, String>;

pub(super) trait AcquireGatePlatform {
    type ClientKey: Clone + Eq + Hash;
    type Client;
    type SurfaceKey: Clone + Eq;
    type Surface: Clone;
    type Point;
    type Source;
    type Blocker;
    type ReleaseHandle;
    type CancelHandle;
    type Registration;

    fn generate(
        &mut self,
        point: &Self::Point,
    ) -> PrepareResult<Self::Source, Self::Blocker, Self::ReleaseHandle, Self::CancelHandle>;

    fn register(
        &mut self,
        gate_id: GateId,
        source: Self::Source,
    ) -> Result<Self::Registration, String>;

    fn add_blocker(&mut self, surface: &Self::Surface, blocker: Self::Blocker);
    fn add_cancelled_blocker(&mut self, surface: &Self::Surface);
    fn reject_client(&mut self, client: &Self::Client, failure: &AcquireGateFailure);
    fn release(&mut self, release_handle: Self::ReleaseHandle);
    fn cancel(&mut self, cancel_handle: Self::CancelHandle);
    fn unregister(&mut self, registration: Self::Registration);
}

struct LiveGate<P: AcquireGatePlatform> {
    client: P::ClientKey,
    // Keep only non-owning identity here. Retaining WlSurface would pin its
    // cache, hooks and pending transaction for the lifetime of an unsignalled gate.
    surface_key: P::SurfaceKey,
    registration: P::Registration,
    release_handle: P::ReleaseHandle,
    cancel_handle: P::CancelHandle,
}

pub(super) struct AcquireGateEngine<P: AcquireGatePlatform> {
    platform: P,
    next_gate_id: u64,
    live: HashMap<GateId, LiveGate<P>>,
    gates_by_client: HashMap<P::ClientKey, usize>,
}

impl<P: AcquireGatePlatform> AcquireGateEngine<P> {
    pub(super) fn new(platform: P) -> Self {
        Self {
            platform,
            next_gate_id: 1,
            live: HashMap::new(),
            gates_by_client: HashMap::new(),
        }
    }

    pub(super) fn prepare_commit(
        &mut self,
        client: P::ClientKey,
        client_handle: &P::Client,
        surface_key: P::SurfaceKey,
        surface: P::Surface,
        acquire_point: Option<P::Point>,
    ) -> AcquireGateDecision {
        let Some(acquire_point) = acquire_point else {
            return AcquireGateDecision::NoAcquirePoint;
        };

        let client_count = self
            .gates_by_client
            .get(&client)
            .copied()
            .unwrap_or_default();
        if client_count >= MAX_CLIENT_ACQUIRE_GATES {
            let failure = AcquireGateFailure::ClientBudget {
                count: client_count,
                limit: MAX_CLIENT_ACQUIRE_GATES,
            };
            self.platform.add_cancelled_blocker(&surface);
            self.platform.reject_client(client_handle, &failure);
            return AcquireGateDecision::Rejected(failure);
        }
        let global_count = self.live.len();
        if global_count >= MAX_GLOBAL_ACQUIRE_GATES {
            let failure = AcquireGateFailure::GlobalBudget {
                count: global_count,
                limit: MAX_GLOBAL_ACQUIRE_GATES,
            };
            self.platform.add_cancelled_blocker(&surface);
            self.platform.reject_client(client_handle, &failure);
            return AcquireGateDecision::Rejected(failure);
        }

        self.gates_by_client
            .insert(client.clone(), client_count + 1);
        let gate_id = self.allocate_gate_id();

        let prepared = match self.platform.generate(&acquire_point) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.release_client_gate(&client);
                let failure = AcquireGateFailure::Generate(error);
                self.platform.add_cancelled_blocker(&surface);
                self.platform.reject_client(client_handle, &failure);
                return AcquireGateDecision::Rejected(failure);
            }
        };
        let registration = match self.platform.register(gate_id, prepared.source) {
            Ok(registration) => registration,
            Err(error) => {
                self.release_client_gate(&client);
                let failure = AcquireGateFailure::Register(error);
                self.platform.add_cancelled_blocker(&surface);
                self.platform.reject_client(client_handle, &failure);
                return AcquireGateDecision::Rejected(failure);
            }
        };

        self.live.insert(
            gate_id,
            LiveGate {
                client,
                surface_key,
                registration,
                release_handle: prepared.release_handle,
                cancel_handle: prepared.cancel_handle,
            },
        );
        self.platform.add_blocker(&surface, prepared.blocker);

        AcquireGateDecision::Armed(gate_id)
    }

    pub(super) fn source_ready(&mut self, gate_id: GateId) -> Option<P::ClientKey> {
        let gate = self.live.remove(&gate_id)?;
        self.release_client_gate(&gate.client);
        Some(gate.client)
    }

    pub(super) fn surface_destroyed(&mut self, surface_key: &P::SurfaceKey) -> Vec<P::ClientKey> {
        let mut gate_ids = self
            .live
            .iter()
            .filter_map(|(gate_id, gate)| (gate.surface_key == *surface_key).then_some(*gate_id))
            .collect::<Vec<_>>();
        gate_ids.sort_unstable();
        let mut wake_clients = Vec::with_capacity(gate_ids.len());
        for gate_id in gate_ids {
            let gate = self
                .live
                .remove(&gate_id)
                .expect("surface gate scan contains a live acquire gate");
            // A surface blocker may have fused into a sibling's transaction.
            // Released is neutral beside Pending; Cancelled would discard the whole transaction.
            self.platform.release(gate.release_handle);
            self.platform.unregister(gate.registration);
            self.release_client_gate(&gate.client);
            wake_clients.push(gate.client);
        }
        wake_clients
    }

    pub(super) fn client_destroyed(&mut self, client: &P::ClientKey) -> usize {
        let mut gate_ids = self
            .live
            .iter()
            .filter_map(|(gate_id, gate)| (gate.client == *client).then_some(*gate_id))
            .collect::<Vec<_>>();
        gate_ids.sort_unstable();
        let removed = gate_ids.len();
        for gate_id in gate_ids {
            let gate = self
                .live
                .remove(&gate_id)
                .expect("client gate scan contains a live acquire gate");
            self.platform.cancel(gate.cancel_handle);
            self.platform.unregister(gate.registration);
            self.release_client_gate(&gate.client);
        }
        removed
    }

    fn allocate_gate_id(&mut self) -> GateId {
        loop {
            let candidate = GateId(self.next_gate_id);
            self.next_gate_id = self.next_gate_id.wrapping_add(1);
            if !self.live.contains_key(&candidate) {
                return candidate;
            }
        }
    }

    fn release_client_gate(&mut self, client: &P::ClientKey) {
        let count = self
            .gates_by_client
            .get_mut(client)
            .expect("reserved acquire gate has client accounting");
        *count = count
            .checked_sub(1)
            .expect("acquire gate client accounting underflow");
        if *count == 0 {
            self.gates_by_client.remove(client);
        }
    }
}

pub(super) struct LinuxAcquireGatePlatform {
    loop_handle: LoopHandle<'static, WaylandState>,
    display_handle: DisplayHandle,
}

impl LinuxAcquireGatePlatform {
    pub(super) fn new(
        loop_handle: LoopHandle<'static, WaylandState>,
        display_handle: DisplayHandle,
    ) -> Self {
        Self {
            loop_handle,
            display_handle,
        }
    }
}

const GATE_OVERRIDE_NONE: u8 = 0;
const GATE_OVERRIDE_RELEASED: u8 = 1;
const GATE_OVERRIDE_CANCELLED: u8 = 2;

struct GateOverrideState(AtomicU8);

pub(super) struct DrmGateReleaseHandle(Arc<GateOverrideState>);
pub(super) struct DrmGateCancelHandle(Arc<GateOverrideState>);

impl DrmGateReleaseHandle {
    fn release(self) {
        self.0.0.store(GATE_OVERRIDE_RELEASED, Ordering::Release);
    }
}

impl DrmGateCancelHandle {
    fn cancel(self) {
        self.0.0.store(GATE_OVERRIDE_CANCELLED, Ordering::Release);
    }
}

pub(super) struct AcquireGateBlocker<B> {
    blocker: B,
    override_state: Arc<GateOverrideState>,
}

impl<B: Blocker> Blocker for AcquireGateBlocker<B> {
    fn state(&self) -> BlockerState {
        match self.override_state.0.load(Ordering::Acquire) {
            GATE_OVERRIDE_RELEASED => BlockerState::Released,
            GATE_OVERRIDE_CANCELLED => BlockerState::Cancelled,
            _ => self.blocker.state(),
        }
    }
}

struct CancelledAcquireBlocker;

impl Blocker for CancelledAcquireBlocker {
    fn state(&self) -> BlockerState {
        BlockerState::Cancelled
    }
}

impl AcquireGatePlatform for LinuxAcquireGatePlatform {
    type ClientKey = ClientId;
    type Client = Client;
    type SurfaceKey = smithay::reexports::wayland_server::backend::ObjectId;
    type Surface = WlSurface;
    type Point = DrmSyncPoint;
    type Source = DrmSyncPointSource;
    type Blocker = AcquireGateBlocker<DrmSyncPointBlocker>;
    type ReleaseHandle = DrmGateReleaseHandle;
    type CancelHandle = DrmGateCancelHandle;
    type Registration = RegistrationToken;

    fn generate(
        &mut self,
        point: &Self::Point,
    ) -> PrepareResult<Self::Source, Self::Blocker, Self::ReleaseHandle, Self::CancelHandle> {
        let (blocker, source) = point
            .generate_blocker()
            .map_err(|error| error.to_string())?;
        let override_state = Arc::new(GateOverrideState(AtomicU8::new(GATE_OVERRIDE_NONE)));
        Ok(PreparedGate {
            source,
            blocker: AcquireGateBlocker {
                blocker,
                override_state: Arc::clone(&override_state),
            },
            release_handle: DrmGateReleaseHandle(Arc::clone(&override_state)),
            cancel_handle: DrmGateCancelHandle(override_state),
        })
    }

    fn register(
        &mut self,
        gate_id: GateId,
        source: Self::Source,
    ) -> Result<Self::Registration, String> {
        self.loop_handle
            .insert_source(source, move |(), (), state: &mut WaylandState| {
                state.acquire_gate_source_ready(gate_id);
                Ok(())
            })
            .map_err(|error| error.to_string())
    }

    fn add_blocker(&mut self, surface: &Self::Surface, blocker: Self::Blocker) {
        compositor::add_blocker(surface, blocker);
    }

    fn add_cancelled_blocker(&mut self, surface: &Self::Surface) {
        compositor::add_blocker(surface, CancelledAcquireBlocker);
    }

    fn reject_client(&mut self, client: &Self::Client, failure: &AcquireGateFailure) {
        terminate_resource_exhausting_client(
            &self.display_handle,
            client,
            format!("explicit-sync acquire gate unavailable: {failure}"),
        );
    }

    fn release(&mut self, release_handle: Self::ReleaseHandle) {
        release_handle.release();
    }

    fn cancel(&mut self, cancel_handle: Self::CancelHandle) {
        cancel_handle.cancel();
    }

    fn unregister(&mut self, registration: Self::Registration) {
        self.loop_handle.remove(registration);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakePoint(u64);

    struct FakeSource(u64);
    struct FakeBlocker(u64);
    struct FakeReleaseHandle(u64);
    struct FakeCancelHandle(u64);
    struct FakeRegistration(u64);

    #[derive(Clone)]
    struct FakeSurface {
        id: u32,
        retention: Arc<()>,
    }

    impl FakeSurface {
        fn new(id: u32) -> Self {
            Self {
                id,
                retention: Arc::new(()),
            }
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum PlatformCall {
        Generate(u64),
        Register(GateId, u64),
        AddBlocker(u32, u64),
        AddCancelledBlocker(u32),
        RejectClient(u32, FailureKind),
        Release(u64),
        Cancel(u64),
        Unregister(u64),
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FailureKind {
        ClientBudget,
        GlobalBudget,
        Generate,
        Register,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FakeGateState {
        Pending,
        Released,
        Cancelled,
    }

    #[derive(Default)]
    struct FakePlatform {
        fail_generate: bool,
        fail_register: bool,
        calls: Vec<PlatformCall>,
        gate_states: HashMap<u64, FakeGateState>,
    }

    impl AcquireGatePlatform for FakePlatform {
        type ClientKey = u32;
        type Client = u32;
        type SurfaceKey = u32;
        type Surface = FakeSurface;
        type Point = FakePoint;
        type Source = FakeSource;
        type Blocker = FakeBlocker;
        type ReleaseHandle = FakeReleaseHandle;
        type CancelHandle = FakeCancelHandle;
        type Registration = FakeRegistration;

        fn generate(
            &mut self,
            point: &Self::Point,
        ) -> PrepareResult<Self::Source, Self::Blocker, Self::ReleaseHandle, Self::CancelHandle>
        {
            self.calls.push(PlatformCall::Generate(point.0));
            if std::mem::take(&mut self.fail_generate) {
                return Err("synthetic generation failure".into());
            }
            self.gate_states.insert(point.0, FakeGateState::Pending);
            Ok(PreparedGate {
                source: FakeSource(point.0),
                blocker: FakeBlocker(point.0),
                release_handle: FakeReleaseHandle(point.0),
                cancel_handle: FakeCancelHandle(point.0),
            })
        }

        fn register(
            &mut self,
            gate_id: GateId,
            source: Self::Source,
        ) -> Result<Self::Registration, String> {
            self.calls.push(PlatformCall::Register(gate_id, source.0));
            if std::mem::take(&mut self.fail_register) {
                return Err("synthetic registration failure".into());
            }
            Ok(FakeRegistration(source.0))
        }

        fn add_blocker(&mut self, surface: &Self::Surface, blocker: Self::Blocker) {
            let _ = Arc::strong_count(&surface.retention);
            self.calls
                .push(PlatformCall::AddBlocker(surface.id, blocker.0));
        }

        fn add_cancelled_blocker(&mut self, surface: &Self::Surface) {
            self.calls
                .push(PlatformCall::AddCancelledBlocker(surface.id));
        }

        fn reject_client(&mut self, client: &Self::Client, failure: &AcquireGateFailure) {
            let kind = match failure {
                AcquireGateFailure::ClientBudget { .. } => FailureKind::ClientBudget,
                AcquireGateFailure::GlobalBudget { .. } => FailureKind::GlobalBudget,
                AcquireGateFailure::Generate(_) => FailureKind::Generate,
                AcquireGateFailure::Register(_) => FailureKind::Register,
            };
            self.calls.push(PlatformCall::RejectClient(*client, kind));
        }

        fn release(&mut self, release_handle: Self::ReleaseHandle) {
            self.calls.push(PlatformCall::Release(release_handle.0));
            self.gate_states
                .insert(release_handle.0, FakeGateState::Released);
        }

        fn cancel(&mut self, cancel_handle: Self::CancelHandle) {
            self.calls.push(PlatformCall::Cancel(cancel_handle.0));
            self.gate_states
                .insert(cancel_handle.0, FakeGateState::Cancelled);
        }

        fn unregister(&mut self, registration: Self::Registration) {
            self.calls.push(PlatformCall::Unregister(registration.0));
        }
    }

    fn engine() -> AcquireGateEngine<FakePlatform> {
        AcquireGateEngine::new(FakePlatform::default())
    }

    fn arm(
        engine: &mut AcquireGateEngine<FakePlatform>,
        client: u32,
        surface: u32,
        point: u64,
    ) -> GateId {
        let AcquireGateDecision::Armed(gate_id) = engine.prepare_commit(
            client,
            &client,
            surface,
            FakeSurface::new(surface),
            Some(FakePoint(point)),
        ) else {
            panic!("fake acquire gate must arm");
        };
        gate_id
    }

    #[test]
    fn gate_limits_match_the_existing_retained_dmabuf_budgets() {
        assert_eq!(MAX_CLIENT_ACQUIRE_GATES, 256);
        assert_eq!(MAX_GLOBAL_ACQUIRE_GATES, 1024);
    }

    #[test]
    fn missing_acquire_point_never_reserves_or_calls_platform() {
        let mut engine = engine();

        let decision = engine.prepare_commit(1, &1, 10, FakeSurface::new(10), None);

        assert_eq!(decision, AcquireGateDecision::NoAcquirePoint);
        assert!(engine.live.is_empty());
        assert!(engine.gates_by_client.is_empty());
        assert!(engine.platform.calls.is_empty());
    }

    #[test]
    fn successful_gate_registers_before_recorded_blocker_is_armed() {
        let mut engine = engine();

        let gate_id = arm(&mut engine, 1, 10, 99);

        assert_eq!(
            engine.platform.calls,
            [
                PlatformCall::Generate(99),
                PlatformCall::Register(gate_id, 99),
                PlatformCall::AddBlocker(10, 99),
            ]
        );
        assert!(engine.live.contains_key(&gate_id));
        assert_eq!(engine.gates_by_client.get(&1), Some(&1));
    }

    #[test]
    fn live_gate_does_not_retain_the_surface_handle() {
        let mut engine = engine();
        let retention = Arc::new(());
        let surface = FakeSurface {
            id: 10,
            retention: Arc::clone(&retention),
        };

        let decision = engine.prepare_commit(1, &1, 10, surface, Some(FakePoint(99)));

        assert!(matches!(decision, AcquireGateDecision::Armed(_)));
        assert_eq!(Arc::strong_count(&retention), 1);
        assert_eq!(engine.live.len(), 1);
    }

    #[test]
    fn gate_identifier_wraps_without_panicking() {
        let mut engine = engine();
        engine.next_gate_id = u64::MAX;

        let last = arm(&mut engine, 1, 10, 99);
        let wrapped = arm(&mut engine, 1, 10, 100);

        assert_eq!(last, GateId(u64::MAX));
        assert_eq!(wrapped, GateId(0));
        assert_eq!(engine.next_gate_id, 1);
        assert_eq!(engine.live.len(), 2);
    }

    #[test]
    fn gate_identifier_skips_a_live_collision() {
        let mut engine = engine();
        engine.next_gate_id = 7;
        let existing = arm(&mut engine, 1, 10, 99);
        engine.next_gate_id = 7;

        let next = arm(&mut engine, 1, 10, 100);

        assert_eq!(existing, GateId(7));
        assert_eq!(next, GateId(8));
        assert_eq!(engine.next_gate_id, 9);
        assert_eq!(engine.live.len(), 2);
    }

    #[test]
    fn client_budget_refuses_only_the_first_gate_beyond_256() {
        let mut engine = engine();
        for index in 0..MAX_CLIENT_ACQUIRE_GATES {
            let _ = arm(&mut engine, 1, index as u32, index as u64);
        }
        engine.platform.calls.clear();

        let decision =
            engine.prepare_commit(1, &1, 9999, FakeSurface::new(9999), Some(FakePoint(9999)));

        assert_eq!(
            decision,
            AcquireGateDecision::Rejected(AcquireGateFailure::ClientBudget {
                count: MAX_CLIENT_ACQUIRE_GATES,
                limit: MAX_CLIENT_ACQUIRE_GATES,
            })
        );
        assert_eq!(
            engine.platform.calls,
            [
                PlatformCall::AddCancelledBlocker(9999),
                PlatformCall::RejectClient(1, FailureKind::ClientBudget),
            ]
        );
        assert_eq!(engine.live.len(), MAX_CLIENT_ACQUIRE_GATES);
    }

    #[test]
    fn global_budget_refuses_only_the_first_gate_beyond_1024() {
        let mut engine = engine();
        for index in 0..MAX_GLOBAL_ACQUIRE_GATES {
            let client = (index / MAX_CLIENT_ACQUIRE_GATES) as u32;
            let _ = arm(&mut engine, client, index as u32, index as u64);
        }
        engine.platform.calls.clear();

        let decision =
            engine.prepare_commit(99, &99, 9999, FakeSurface::new(9999), Some(FakePoint(9999)));

        assert_eq!(
            decision,
            AcquireGateDecision::Rejected(AcquireGateFailure::GlobalBudget {
                count: MAX_GLOBAL_ACQUIRE_GATES,
                limit: MAX_GLOBAL_ACQUIRE_GATES,
            })
        );
        assert_eq!(
            engine.platform.calls,
            [
                PlatformCall::AddCancelledBlocker(9999),
                PlatformCall::RejectClient(99, FailureKind::GlobalBudget),
            ]
        );
        assert_eq!(engine.live.len(), MAX_GLOBAL_ACQUIRE_GATES);
    }

    #[test]
    fn generation_failure_cancels_commit_and_restores_budget() {
        let mut engine = engine();
        engine.platform.fail_generate = true;

        let decision = engine.prepare_commit(1, &1, 10, FakeSurface::new(10), Some(FakePoint(99)));

        assert_eq!(
            decision,
            AcquireGateDecision::Rejected(AcquireGateFailure::Generate(
                "synthetic generation failure".into()
            ))
        );
        assert_eq!(
            engine.platform.calls,
            [
                PlatformCall::Generate(99),
                PlatformCall::AddCancelledBlocker(10),
                PlatformCall::RejectClient(1, FailureKind::Generate),
            ]
        );
        assert!(engine.live.is_empty());
        assert!(engine.gates_by_client.is_empty());
        let _ = arm(&mut engine, 1, 10, 100);
        assert_eq!(engine.gates_by_client.get(&1), Some(&1));
    }

    #[test]
    fn registration_failure_never_arms_pending_blocker_and_restores_budget() {
        let mut engine = engine();
        engine.platform.fail_register = true;

        let decision = engine.prepare_commit(1, &1, 10, FakeSurface::new(10), Some(FakePoint(99)));

        assert_eq!(
            decision,
            AcquireGateDecision::Rejected(AcquireGateFailure::Register(
                "synthetic registration failure".into()
            ))
        );
        assert_eq!(
            engine.platform.calls,
            [
                PlatformCall::Generate(99),
                PlatformCall::Register(GateId(1), 99),
                PlatformCall::AddCancelledBlocker(10),
                PlatformCall::RejectClient(1, FailureKind::Register),
            ]
        );
        assert!(engine.live.is_empty());
        assert!(engine.gates_by_client.is_empty());
        let _ = arm(&mut engine, 1, 10, 100);
        assert_eq!(engine.gates_by_client.get(&1), Some(&1));
    }

    #[test]
    fn source_ready_releases_exact_gate_without_unregistering_live_callback() {
        let mut engine = engine();
        let gate_id = arm(&mut engine, 1, 10, 99);
        engine.platform.calls.clear();

        let wake = engine.source_ready(gate_id);

        assert_eq!(wake, Some(1));
        assert!(engine.live.is_empty());
        assert!(engine.gates_by_client.is_empty());
        assert!(engine.platform.calls.is_empty());
    }

    #[test]
    fn stale_source_ready_is_effectless_and_cannot_underflow_budget() {
        let mut engine = engine();
        let gate_id = arm(&mut engine, 1, 10, 99);
        assert_eq!(engine.source_ready(gate_id), Some(1));
        engine.platform.calls.clear();

        assert_eq!(engine.source_ready(gate_id), None);
        assert!(engine.live.is_empty());
        assert!(engine.gates_by_client.is_empty());
        assert!(engine.platform.calls.is_empty());
    }

    #[test]
    fn one_ready_gate_keeps_other_client_gate_live() {
        let mut engine = engine();
        let first = arm(&mut engine, 1, 10, 1);
        let second = arm(&mut engine, 1, 10, 2);

        assert_eq!(engine.source_ready(first), Some(1));

        assert_eq!(engine.gates_by_client.get(&1), Some(&1));
        assert!(engine.live.contains_key(&second));
    }

    #[test]
    fn destroying_one_surface_releases_only_its_gate_and_leaves_fused_sibling_pending() {
        let mut engine = engine();
        let destroyed = arm(&mut engine, 1, 10, 1);
        let sibling = arm(&mut engine, 1, 20, 2);
        let other_client = arm(&mut engine, 2, 30, 3);
        engine.platform.calls.clear();

        let wake_clients = engine.surface_destroyed(&10);

        assert_eq!(wake_clients, [1]);
        assert_eq!(
            engine.platform.calls,
            [PlatformCall::Release(1), PlatformCall::Unregister(1)]
        );
        assert!(!engine.live.contains_key(&destroyed));
        assert!(engine.live.contains_key(&sibling));
        assert!(engine.live.contains_key(&other_client));
        assert_eq!(
            engine.platform.gate_states.get(&1),
            Some(&FakeGateState::Released)
        );
        assert_eq!(
            engine.platform.gate_states.get(&2),
            Some(&FakeGateState::Pending)
        );
        assert_eq!(
            engine.platform.gate_states.get(&3),
            Some(&FakeGateState::Pending)
        );
        assert_eq!(engine.gates_by_client.get(&1), Some(&1));
        assert_eq!(engine.gates_by_client.get(&2), Some(&1));
        assert_eq!(engine.live.len(), 2);
    }

    #[test]
    fn destroying_surface_without_gates_is_effectless() {
        let mut engine = engine();

        assert!(engine.surface_destroyed(&10).is_empty());
        assert!(engine.live.is_empty());
        assert!(engine.gates_by_client.is_empty());
        assert!(engine.platform.calls.is_empty());
    }

    #[test]
    fn client_destruction_cancels_then_unregisters_its_gates_and_releases_budget() {
        let mut engine = engine();
        let _ = arm(&mut engine, 1, 10, 1);
        let _ = arm(&mut engine, 1, 10, 2);
        let other = arm(&mut engine, 2, 20, 3);
        engine.platform.calls.clear();

        let removed = engine.client_destroyed(&1);

        assert_eq!(removed, 2);
        assert_eq!(
            engine.platform.calls,
            [
                PlatformCall::Cancel(1),
                PlatformCall::Unregister(1),
                PlatformCall::Cancel(2),
                PlatformCall::Unregister(2),
            ]
        );
        assert_eq!(engine.live.len(), 1);
        assert!(!engine.gates_by_client.contains_key(&1));
        assert_eq!(engine.gates_by_client.get(&2), Some(&1));
        assert!(engine.live.contains_key(&other));
        assert_eq!(
            engine.platform.gate_states.get(&1),
            Some(&FakeGateState::Cancelled)
        );
        assert_eq!(
            engine.platform.gate_states.get(&2),
            Some(&FakeGateState::Cancelled)
        );
        assert_eq!(
            engine.platform.gate_states.get(&3),
            Some(&FakeGateState::Pending)
        );
    }

    #[test]
    fn destroying_client_without_gates_is_effectless() {
        let mut engine = engine();

        assert_eq!(engine.client_destroyed(&10), 0);
        assert!(engine.live.is_empty());
        assert!(engine.gates_by_client.is_empty());
        assert!(engine.platform.calls.is_empty());
    }

    struct TestBlocker(BlockerState);

    impl Blocker for TestBlocker {
        fn state(&self) -> BlockerState {
            self.0
        }
    }

    #[test]
    fn acquire_gate_blocker_forwards_pending_and_released_inner_states() {
        for expected in [BlockerState::Pending, BlockerState::Released] {
            let blocker = AcquireGateBlocker {
                blocker: TestBlocker(expected),
                override_state: Arc::new(GateOverrideState(AtomicU8::new(GATE_OVERRIDE_NONE))),
            };
            assert_eq!(blocker.state(), expected);
        }
    }

    #[test]
    fn surface_release_override_is_released_never_cancelled() {
        let override_state = Arc::new(GateOverrideState(AtomicU8::new(GATE_OVERRIDE_NONE)));
        let blocker = AcquireGateBlocker {
            blocker: TestBlocker(BlockerState::Pending),
            override_state: Arc::clone(&override_state),
        };
        DrmGateReleaseHandle(override_state).release();

        assert_eq!(blocker.state(), BlockerState::Released);
    }

    #[test]
    fn linux_platform_release_delegates_to_release_handle() {
        use smithay::reexports::{calloop::EventLoop, wayland_server::Display};

        let event_loop = EventLoop::<WaylandState>::try_new().expect("construct test event loop");
        let display = Display::<WaylandState>::new().expect("construct test display");
        let mut platform = LinuxAcquireGatePlatform::new(event_loop.handle(), display.handle());
        let override_state = Arc::new(GateOverrideState(AtomicU8::new(GATE_OVERRIDE_NONE)));

        platform.release(DrmGateReleaseHandle(Arc::clone(&override_state)));

        assert_eq!(
            override_state.0.load(Ordering::Acquire),
            GATE_OVERRIDE_RELEASED
        );
    }

    #[test]
    fn client_cancellation_overrides_released_inner_blocker() {
        let override_state = Arc::new(GateOverrideState(AtomicU8::new(GATE_OVERRIDE_NONE)));
        let blocker = AcquireGateBlocker {
            blocker: TestBlocker(BlockerState::Released),
            override_state: Arc::clone(&override_state),
        };
        DrmGateCancelHandle(override_state).cancel();

        assert_eq!(blocker.state(), BlockerState::Cancelled);
    }

    #[test]
    fn cancelled_blocker_is_always_cancelled() {
        assert_eq!(CancelledAcquireBlocker.state(), BlockerState::Cancelled);
    }
}
