use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::Duration;

use bevy::app::{App, AppExit, Plugin, Update};
use bevy::ecs::message::{MessageReader, MessageWriter};
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::{IntoScheduleConfigs, SystemSet};
use bevy::log::{error, warn};
use bevy::prelude::{Res, ResMut, Time, Timer, TimerMode};
use cosmix_interaction_schema::{
    DialogPresentationV1, DialogPresenterFailRequestV1, DialogPresenterLeaseV1,
    DialogPresenterMarkPresentedRequestV1, DialogPresenterNextRequestV1,
    DialogPresenterNextResponseV1, DialogPresenterProgressCancelRequestV1,
    DialogPresenterRegisterRequestV1, DialogPresenterRegisterResponseV1,
    DialogPresenterReleaseRequestV1, DialogPresenterResolveRequestV1, DialogRequestV1,
    TOPIC_INTERACT_PROPS_CHANGED, VERB_PRESENTER_FAIL, VERB_PRESENTER_MARK_PRESENTED,
    VERB_PRESENTER_NEXT, VERB_PRESENTER_PROGRESS_CANCEL, VERB_PRESENTER_REGISTER,
    VERB_PRESENTER_RELEASE, VERB_PRESENTER_RESOLVE,
};
use ctk::prelude::{
    BusBridge, BusBridgeEvent, BusConnectionState, BusMessage, BusReply, FileRequest,
    FileRequestId, FileRequestResult, FileRequesterSystems, InteractionId, InteractionOutcome,
    InteractionResult, InteractionSystems, InteractionValue, ProgressComplete, ProgressCompletion,
    ProgressUpdate, ProgressValue, WithdrawFileRequest, WithdrawInteraction,
};
use serde::Serialize;
use serde_json::Value;

use crate::mapping::{self, UiEmission, UiOutcome};

const INTERACT_SERVICE: &str = "interact";
const PROPS_GET_VERB: &str = "interact.props.get";
const PROGRESS_RATIO_TOTAL: u64 = 10_000;
const BACKSTOP_SECONDS: f32 = 300.0;
const PROGRESS_CANCEL_RETENTION: Duration = Duration::from_secs(30);
const RESEED_RETRY_BASE: Duration = Duration::from_millis(250);
const RESEED_RETRY_MAX: Duration = Duration::from_secs(5);

#[derive(SystemSet, Clone, Debug, PartialEq, Eq, Hash)]
enum PresenterSystems {
    BusIngress,
    Drive,
    CollectResults,
    ResultDrive,
    Backstop,
    Exit,
}

pub(crate) struct PresenterPlugin;

impl Plugin for PresenterPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PresenterState>()
            .init_resource::<RequestIds>()
            .init_resource::<BackstopTimer>()
            .configure_sets(
                Update,
                (
                    PresenterSystems::BusIngress,
                    PresenterSystems::Drive,
                    InteractionSystems,
                    FileRequesterSystems,
                    PresenterSystems::CollectResults,
                    PresenterSystems::ResultDrive,
                    PresenterSystems::Backstop,
                    PresenterSystems::Exit,
                )
                    .chain(),
            )
            .add_systems(Update, bus_ingress.in_set(PresenterSystems::BusIngress))
            .add_systems(Update, presenter_drive.in_set(PresenterSystems::Drive))
            .add_systems(
                Update,
                collect_ui_results.in_set(PresenterSystems::CollectResults),
            )
            .add_systems(
                Update,
                presenter_result_drive.in_set(PresenterSystems::ResultDrive),
            )
            .add_systems(
                Update,
                (gc_cancelled_progress, backstop_wake)
                    .chain()
                    .in_set(PresenterSystems::Backstop),
            )
            .add_systems(Update, on_app_exit.in_set(PresenterSystems::Exit));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum UiKey {
    Interaction(InteractionId),
    File(FileRequestId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PresentationPhase {
    AwaitingMark,
    Showing,
    TerminalPending,
    ProgressCancelPending,
    ProgressCancelAwaitingOwner,
}

#[derive(Debug)]
pub(crate) struct ActivePresentation {
    pub(crate) handle: String,
    pub(crate) attempt_token: u64,
    pub(crate) original_request: DialogRequestV1,
    pub(crate) phase: PresentationPhase,
    pub(crate) pending_outcome: Option<UiOutcome>,
    created_seq: u64,
    needs_rebind: bool,
    cancel_gc_deadline: Option<Duration>,
}

#[derive(Clone, Debug)]
enum PendingRpc {
    Register,
    Next,
    Mark { key: UiKey },
    Resolve { key: UiKey },
    Fail { key: Option<UiKey> },
    ProgressCancel { key: UiKey },
    Reseed,
    Release,
}

impl PendingRpc {
    fn key(&self) -> Option<UiKey> {
        match self {
            Self::Mark { key } | Self::Resolve { key } | Self::ProgressCancel { key } => Some(*key),
            Self::Fail { key } => *key,
            Self::Register | Self::Next | Self::Reseed | Self::Release => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SurfaceRetirement {
    Interaction(InteractionId),
    File(FileRequestId),
    Progress(InteractionId, ProgressCompletion),
}

#[derive(Resource, Default)]
pub(crate) struct PresenterState {
    pub(crate) lease: Option<DialogPresenterLeaseV1>,
    pub(crate) bridge_generation: Option<u64>,
    pending_rpc: HashMap<u64, PendingRpc>,
    pub(crate) by_ui: HashMap<UiKey, ActivePresentation>,
    pub(crate) by_handle: HashMap<String, UiKey>,
    pub(crate) next_in_flight: bool,
    pub(crate) drain_requested: bool,
    ready_presentations: VecDeque<DialogPresentationV1>,
    connected: bool,
    reseed_requested: bool,
    reseed_in_flight: bool,
    reseed_dirty: bool,
    reseed_not_before: Duration,
    reseed_failures: u32,
    surface_seq: u64,
    reseed_inflight_watermark: u64,
}

impl PresenterState {
    fn next_surface_seq(&mut self) -> u64 {
        self.surface_seq = self
            .surface_seq
            .checked_add(1)
            .expect("interact-gui surface sequence exhausted");
        self.surface_seq
    }

    fn remove_key(&mut self, key: UiKey) -> Option<ActivePresentation> {
        let active = self.by_ui.remove(&key)?;
        if self.by_handle.get(&active.handle) == Some(&key) {
            self.by_handle.remove(&active.handle);
        } else {
            warn!(
                handle = %active.handle,
                ?key,
                "interact-gui correlation reverse entry was inconsistent during removal"
            );
        }
        self.pending_rpc
            .retain(|_, pending| pending.key() != Some(key));
        Some(active)
    }

    fn insert_correlation(&mut self, key: UiKey, active: ActivePresentation) -> bool {
        match self.by_ui.entry(key) {
            Entry::Vacant(slot) => {
                self.by_handle.insert(active.handle.clone(), key);
                slot.insert(active);
                true
            }
            Entry::Occupied(existing) => {
                warn!(
                    ?key,
                    existing_handle = %existing.get().handle,
                    rejected_handle = %active.handle,
                    "interact-gui UI correlation collision; rejecting new presentation"
                );
                false
            }
        }
    }

    fn store_outcome(&mut self, key: UiKey, outcome: UiOutcome) {
        let Some(active) = self.by_ui.get_mut(&key) else {
            return;
        };
        let route = result_route(&active.original_request, &outcome);
        if route == ResultRoute::IgnoreProgressCompletion {
            return;
        }
        active.pending_outcome = Some(outcome);
        if route == ResultRoute::ProgressCancel && active.phase != PresentationPhase::AwaitingMark {
            active.phase = PresentationPhase::ProgressCancelPending;
        }
    }

    fn mark_acknowledged(&mut self, key: UiKey) {
        if let Some(active) = self.by_ui.get_mut(&key) {
            active.phase = if active.cancel_gc_deadline.is_some() {
                PresentationPhase::ProgressCancelAwaitingOwner
            } else if active
                .pending_outcome
                .as_ref()
                .is_some_and(is_progress_cancel)
            {
                PresentationPhase::ProgressCancelPending
            } else {
                PresentationPhase::Showing
            };
        }
    }

    fn reconcile_existing(&mut self, presentation: &DialogPresentationV1) -> bool {
        let Some(key) = self.by_handle.get(&presentation.handle).copied() else {
            return false;
        };
        let consistent = self
            .by_ui
            .get(&key)
            .is_some_and(|active| active.handle == presentation.handle);
        if !consistent {
            warn!(
                handle = %presentation.handle,
                ?key,
                "interact-gui repaired an inconsistent reverse correlation"
            );
            self.by_handle.remove(&presentation.handle);
            return false;
        }
        let Some(active) = self.by_ui.get_mut(&key) else {
            return false;
        };
        if active.attempt_token == presentation.attempt_token && !active.needs_rebind {
            return true;
        }
        active.attempt_token = presentation.attempt_token;
        active.original_request = presentation.dialog.clone();
        active.phase = PresentationPhase::AwaitingMark;
        active.needs_rebind = false;
        true
    }

    fn schedule_reseed_retry(&mut self, now: Duration) {
        self.reseed_in_flight = false;
        self.reseed_requested = true;
        self.reseed_failures = self.reseed_failures.saturating_add(1);
        let exponent = self.reseed_failures.saturating_sub(1).min(8);
        let multiplier = 1_u32 << exponent;
        let delay = RESEED_RETRY_BASE
            .saturating_mul(multiplier)
            .min(RESEED_RETRY_MAX);
        self.reseed_not_before = now.saturating_add(delay);
    }

    fn request_reseed(&mut self) {
        if self.reseed_in_flight {
            self.reseed_dirty = true;
        }
        self.reseed_requested = true;
    }

    fn complete_reseed(&mut self) {
        self.reseed_in_flight = false;
        self.reseed_requested = std::mem::take(&mut self.reseed_dirty);
        self.reseed_failures = 0;
        self.reseed_not_before = Duration::ZERO;
    }

    fn drop_transport_inflight(&mut self) {
        for pending in self.pending_rpc.values() {
            reset_active_for_retry(pending, &mut self.by_ui);
        }
        self.pending_rpc.clear();
        self.next_in_flight = false;
        self.reseed_in_flight = false;
        self.reseed_dirty = false;
    }

    fn invalidate_lease(&mut self) {
        self.lease = None;
        self.next_in_flight = false;
        self.reseed_in_flight = false;
        self.reseed_dirty = false;
        self.pending_rpc.clear();
        self.drain_requested = true;
        self.request_reseed();
        for active in self.by_ui.values_mut() {
            active.needs_rebind = true;
            active.phase = PresentationPhase::AwaitingMark;
        }
    }
}

#[derive(Resource)]
struct RequestIds {
    next_bus: u64,
    next_file: u64,
}

impl Default for RequestIds {
    fn default() -> Self {
        Self {
            next_bus: 1,
            next_file: 1,
        }
    }
}

impl RequestIds {
    fn bus(&mut self) -> Option<u64> {
        let value = self.next_bus;
        self.next_bus = self.next_bus.checked_add(1)?;
        Some(value)
    }

    fn file(&mut self) -> Option<FileRequestId> {
        let value = self.next_file;
        self.next_file = self.next_file.checked_add(1)?;
        Some(FileRequestId(value))
    }
}

#[derive(Resource)]
struct BackstopTimer(Timer);

impl Default for BackstopTimer {
    fn default() -> Self {
        Self(Timer::new(
            Duration::from_secs_f32(BACKSTOP_SECONDS),
            TimerMode::Repeating,
        ))
    }
}

fn bus_ingress(
    bridge: Res<BusBridge>,
    time: Res<Time>,
    mut state: ResMut<PresenterState>,
    mut progress_updates: MessageWriter<ProgressUpdate>,
    mut progress_completions: MessageWriter<ProgressComplete>,
    mut interaction_withdrawals: MessageWriter<WithdrawInteraction>,
    mut file_withdrawals: MessageWriter<WithdrawFileRequest>,
) {
    let now = time.elapsed();
    let mut retirements = Vec::new();
    let events: Vec<_> = bridge.drain_events().collect();
    for event in events {
        match event {
            BusBridgeEvent::Connection {
                state: status,
                generation,
            } => {
                handle_connection(status, generation, &mut state);
            }
            BusBridgeEvent::Reply { request_id, result } => {
                let Some(pending) = state.pending_rpc.remove(&request_id) else {
                    continue;
                };
                handle_rpc_reply(
                    pending,
                    result,
                    &mut state,
                    &mut progress_updates,
                    &mut retirements,
                    now,
                );
            }
            BusBridgeEvent::DroppedMessages(count) => {
                warn!(count, "interact-gui Bus topic messages dropped; reseeding");
                state.request_reseed();
                state.drain_requested = true;
            }
            BusBridgeEvent::Fatal(message) => {
                error!("interact-gui Bus bridge failed: {message}");
                state.connected = false;
            }
            BusBridgeEvent::ObservationConnection { .. }
            | BusBridgeEvent::ObservationReply { .. }
            | BusBridgeEvent::ObservationDroppedMessages(_) => {}
        }
    }

    let messages: Vec<_> = bridge.drain_messages().collect();
    for message in messages {
        if message.topic() != Some(TOPIC_INTERACT_PROPS_CHANGED) {
            continue;
        }
        state.drain_requested = true;
        handle_props_message(
            &message,
            &mut state,
            &mut progress_updates,
            &mut retirements,
        );
    }

    flush_surface_retirements(
        retirements,
        &mut progress_completions,
        &mut interaction_withdrawals,
        &mut file_withdrawals,
    );
}

fn handle_connection(status: BusConnectionState, generation: u64, state: &mut PresenterState) {
    if status != BusConnectionState::Connected {
        state.connected = false;
        return;
    }
    if state.bridge_generation != Some(generation) {
        state.drop_transport_inflight();
    }
    state.bridge_generation = Some(generation);
    state.connected = true;
    state.drain_requested = true;
    state.request_reseed();
    state.reseed_not_before = Duration::ZERO;
    state.reseed_failures = 0;
}

fn handle_rpc_reply(
    pending: PendingRpc,
    result: Result<BusReply, String>,
    state: &mut PresenterState,
    progress_updates: &mut MessageWriter<ProgressUpdate>,
    retirements: &mut Vec<SurfaceRetirement>,
    now: Duration,
) {
    let reply = match result {
        Ok(reply) => reply,
        Err(message) => {
            reset_rpc_for_retry(&pending, state);
            if matches!(&pending, PendingRpc::Reseed) {
                state.schedule_reseed_retry(now);
            }
            warn!("interact-gui Bus call failed: {message}");
            return;
        }
    };
    if reply.rc != 0 {
        let error_id = reply_error_id(&reply.body);
        reset_rpc_flag(&pending, state);
        if error_id.as_deref() == Some("stale_lease") {
            state.invalidate_lease();
            return;
        }
        if matches!(&pending, PendingRpc::Reseed) {
            state.schedule_reseed_retry(now);
        }
        handle_application_error(pending, error_id.as_deref(), state, retirements);
        return;
    }

    match pending {
        PendingRpc::Register => {
            match serde_json::from_str::<DialogPresenterRegisterResponseV1>(&reply.body) {
                Ok(response) => {
                    state.lease = Some(response.lease);
                    state.drain_requested = true;
                    state.request_reseed();
                }
                Err(error) => {
                    warn!("invalid presenter-register reply: {error}");
                }
            }
        }
        PendingRpc::Next => {
            state.next_in_flight = false;
            match serde_json::from_str::<DialogPresenterNextResponseV1>(&reply.body) {
                Ok(response) => {
                    if let Some(presentation) = response.presentation {
                        state.ready_presentations.push_back(presentation);
                        state.drain_requested = true;
                    } else {
                        state.drain_requested = false;
                    }
                }
                Err(error) => warn!("invalid presenter-next reply: {error}"),
            }
        }
        PendingRpc::Mark { key } => state.mark_acknowledged(key),
        PendingRpc::Resolve { key } => {
            state.remove_key(key);
            state.drain_requested = true;
        }
        PendingRpc::Fail { key } => {
            if let Some(key) = key {
                state.remove_key(key);
            }
            state.drain_requested = true;
        }
        PendingRpc::ProgressCancel { key } => {
            acknowledge_progress_cancel(key, now, state);
        }
        PendingRpc::Reseed => {
            state.reseed_in_flight = false;
            match serde_json::from_str::<Value>(&reply.body) {
                Ok(snapshot) => {
                    let watermark = state.reseed_inflight_watermark;
                    if apply_props_snapshot(
                        &snapshot,
                        watermark,
                        state,
                        progress_updates,
                        retirements,
                    ) {
                        state.complete_reseed();
                    } else {
                        warn!("interact.props.get reply is not a JSON object");
                        state.schedule_reseed_retry(now);
                    }
                }
                Err(error) => {
                    warn!("invalid interact.props.get reply: {error}");
                    state.schedule_reseed_retry(now);
                }
            }
        }
        PendingRpc::Release => {}
    }
}

fn acknowledge_progress_cancel(key: UiKey, now: Duration, state: &mut PresenterState) {
    if let Some(active) = state.by_ui.get_mut(&key) {
        active.pending_outcome = None;
        // CTK's shipped in-process cancel UX closes the card immediately. The
        // wire owner remains solely responsible for terminal state, so retain
        // only a bounded local correlation.
        active.phase = PresentationPhase::ProgressCancelAwaitingOwner;
        active.cancel_gc_deadline = Some(now.saturating_add(PROGRESS_CANCEL_RETENTION));
    }
}

fn reset_rpc_flag(pending: &PendingRpc, state: &mut PresenterState) {
    match pending {
        PendingRpc::Next => state.next_in_flight = false,
        PendingRpc::Reseed => state.reseed_in_flight = false,
        _ => {}
    }
}

fn reset_rpc_for_retry(pending: &PendingRpc, state: &mut PresenterState) {
    reset_rpc_flag(pending, state);
    reset_active_for_retry(pending, &mut state.by_ui);
}

fn reset_active_for_retry(
    pending: &PendingRpc,
    active_presentations: &mut HashMap<UiKey, ActivePresentation>,
) {
    let key = match pending {
        PendingRpc::Resolve { key }
        | PendingRpc::ProgressCancel { key }
        | PendingRpc::Mark { key } => Some(*key),
        PendingRpc::Fail { key } => *key,
        PendingRpc::Register | PendingRpc::Next | PendingRpc::Reseed | PendingRpc::Release => None,
    };
    let Some(key) = key else {
        return;
    };
    let Some(active) = active_presentations.get_mut(&key) else {
        return;
    };
    active.phase = if active.pending_outcome.as_ref().is_some_and(|outcome| {
        result_route(&active.original_request, outcome) == ResultRoute::ProgressCancel
    }) {
        PresentationPhase::ProgressCancelPending
    } else if matches!(pending, PendingRpc::Mark { .. }) {
        PresentationPhase::AwaitingMark
    } else {
        PresentationPhase::Showing
    };
}

fn handle_application_error(
    pending: PendingRpc,
    error_id: Option<&str>,
    state: &mut PresenterState,
    retirements: &mut Vec<SurfaceRetirement>,
) {
    match pending {
        PendingRpc::Next => {
            state.next_in_flight = false;
            state.drain_requested = false;
        }
        PendingRpc::Mark { key } => match error_id {
            Some("invalid_state") => state.mark_acknowledged(key),
            Some("stale_attempt" | "already_terminal" | "dialog_expired") => {
                retire_presentation(key, None, state, retirements);
            }
            _ => {
                retire_presentation(key, None, state, retirements);
                state.drain_requested = true;
            }
        },
        PendingRpc::Resolve { key } | PendingRpc::ProgressCancel { key } => {
            if matches!(
                error_id,
                Some("stale_attempt" | "already_terminal" | "dialog_expired")
            ) {
                retire_presentation(key, None, state, retirements);
            } else if let Some(active) = state.by_ui.get_mut(&key) {
                active.phase = PresentationPhase::Showing;
            }
            state.drain_requested = true;
        }
        PendingRpc::Fail { key } => {
            if let Some(key) = key {
                retire_presentation(key, None, state, retirements);
            }
            state.drain_requested = true;
        }
        PendingRpc::Register | PendingRpc::Reseed | PendingRpc::Release => {}
    }
}

fn presenter_drive(
    bridge: Res<BusBridge>,
    time: Res<Time>,
    mut state: ResMut<PresenterState>,
    mut ids: ResMut<RequestIds>,
    mut interactions: MessageWriter<ctk::prelude::InteractionRequest>,
    mut files: MessageWriter<FileRequest>,
) {
    if !state.connected {
        return;
    }

    if state.lease.is_none() {
        if !state
            .pending_rpc
            .values()
            .any(|pending| matches!(pending, PendingRpc::Register))
        {
            issue_rpc(
                &bridge,
                &mut ids,
                &mut state,
                PendingRpc::Register,
                VERB_PRESENTER_REGISTER,
                &DialogPresenterRegisterRequestV1 {},
            );
        }
        return;
    }

    let now = time.elapsed();
    if state.reseed_requested && !state.reseed_in_flight && now >= state.reseed_not_before {
        state.reseed_inflight_watermark = state.surface_seq;
        if issue_raw_rpc(
            &bridge,
            &mut ids,
            &mut state,
            PendingRpc::Reseed,
            PROPS_GET_VERB,
            "{}".into(),
        ) {
            state.reseed_requested = false;
            state.reseed_in_flight = true;
            state.reseed_dirty = false;
        } else {
            state.schedule_reseed_retry(now);
        }
    }

    while let Some(presentation) = state.ready_presentations.pop_front() {
        accept_presentation(
            presentation,
            &bridge,
            &mut state,
            &mut ids,
            &mut interactions,
            &mut files,
        );
    }

    let awaiting_mark: Vec<_> = state
        .by_ui
        .iter()
        .filter_map(|(key, active)| {
            (active.phase == PresentationPhase::AwaitingMark && !active.needs_rebind)
                .then_some(*key)
        })
        .collect();
    for key in awaiting_mark {
        issue_mark(&bridge, &mut ids, &mut state, key);
    }

    if state.drain_requested && !state.next_in_flight {
        let lease = state.lease.clone().expect("lease checked above");
        if issue_rpc(
            &bridge,
            &mut ids,
            &mut state,
            PendingRpc::Next,
            VERB_PRESENTER_NEXT,
            &DialogPresenterNextRequestV1 { lease },
        ) {
            state.next_in_flight = true;
        }
    }
}

fn accept_presentation(
    presentation: DialogPresentationV1,
    bridge: &BusBridge,
    state: &mut PresenterState,
    ids: &mut RequestIds,
    interactions: &mut MessageWriter<ctk::prelude::InteractionRequest>,
    files: &mut MessageWriter<FileRequest>,
) {
    if state.reconcile_existing(&presentation) {
        return;
    }

    let mapped = catch_unwind(AssertUnwindSafe(|| mapping::ingress(&presentation)));
    let mut emission = match mapped {
        Ok(Ok(emission)) => emission,
        Ok(Err(error)) => {
            warn!(handle = %presentation.handle, "cannot map dialog presentation: {error}");
            issue_untracked_fail(bridge, ids, state, &presentation);
            return;
        }
        Err(_) => {
            warn!(handle = %presentation.handle, "dialog ingress mapping panicked");
            issue_untracked_fail(bridge, ids, state, &presentation);
            return;
        }
    };

    let key = match &mut emission {
        UiEmission::Interaction(request) => UiKey::Interaction(request.id()),
        UiEmission::File(request) => {
            let Some(id) = ids.file() else {
                error!("interact-gui file request id counter exhausted");
                issue_untracked_fail(bridge, ids, state, &presentation);
                return;
            };
            request.id = id;
            UiKey::File(id)
        }
    };
    let created_seq = state.next_surface_seq();
    let active = ActivePresentation {
        handle: presentation.handle.clone(),
        attempt_token: presentation.attempt_token,
        original_request: presentation.dialog.clone(),
        phase: PresentationPhase::AwaitingMark,
        pending_outcome: None,
        created_seq,
        needs_rebind: false,
        cancel_gc_deadline: None,
    };
    if !state.insert_correlation(key, active) {
        issue_untracked_fail(bridge, ids, state, &presentation);
        return;
    }
    match emission {
        UiEmission::Interaction(request) => {
            interactions.write(request);
        }
        UiEmission::File(request) => {
            files.write(request);
        }
    }
}

fn issue_mark(bridge: &BusBridge, ids: &mut RequestIds, state: &mut PresenterState, key: UiKey) {
    if state.pending_rpc.values().any(
        |pending| matches!(pending, PendingRpc::Mark { key: pending_key } if *pending_key == key),
    ) {
        return;
    }
    let Some(lease) = state.lease.clone() else {
        return;
    };
    let Some(active) = state.by_ui.get(&key) else {
        return;
    };
    let request = DialogPresenterMarkPresentedRequestV1 {
        lease,
        handle: active.handle.clone(),
        attempt_token: active.attempt_token,
    };
    issue_rpc(
        bridge,
        ids,
        state,
        PendingRpc::Mark { key },
        VERB_PRESENTER_MARK_PRESENTED,
        &request,
    );
}

fn issue_untracked_fail(
    bridge: &BusBridge,
    ids: &mut RequestIds,
    state: &mut PresenterState,
    presentation: &DialogPresentationV1,
) {
    let Some(lease) = state.lease.clone() else {
        return;
    };
    let request = DialogPresenterFailRequestV1 {
        lease,
        handle: presentation.handle.clone(),
        attempt_token: presentation.attempt_token,
    };
    issue_rpc(
        bridge,
        ids,
        state,
        PendingRpc::Fail { key: None },
        VERB_PRESENTER_FAIL,
        &request,
    );
}

fn collect_ui_results(
    mut interaction_results: MessageReader<InteractionResult>,
    mut file_results: MessageReader<FileRequestResult>,
    mut state: ResMut<PresenterState>,
) {
    for result in interaction_results.read() {
        state.store_outcome(
            UiKey::Interaction(result.id),
            UiOutcome::Interaction(copy_interaction_outcome(&result.outcome)),
        );
    }
    for result in file_results.read() {
        state.store_outcome(
            UiKey::File(result.id),
            UiOutcome::File(result.outcome.clone()),
        );
    }
}

fn presenter_result_drive(
    bridge: Res<BusBridge>,
    mut state: ResMut<PresenterState>,
    mut ids: ResMut<RequestIds>,
) {
    if !state.connected || state.lease.is_none() {
        return;
    }
    let keys: Vec<_> = state
        .by_ui
        .iter()
        .filter_map(|(key, active)| {
            (active.pending_outcome.is_some()
                && matches!(
                    active.phase,
                    PresentationPhase::Showing | PresentationPhase::ProgressCancelPending
                )
                && !has_terminal_rpc(&state, *key))
            .then_some(*key)
        })
        .collect();

    for key in keys {
        drive_result(&bridge, &mut ids, &mut state, key);
    }
}

fn drive_result(bridge: &BusBridge, ids: &mut RequestIds, state: &mut PresenterState, key: UiKey) {
    let Some(active) = state.by_ui.get_mut(&key) else {
        return;
    };
    let Some(outcome) = active.pending_outcome.as_ref().map(copy_ui_outcome) else {
        return;
    };
    let lease = state.lease.clone().expect("lease checked by caller");
    let handle = active.handle.clone();
    let attempt_token = active.attempt_token;
    let request = active.original_request.clone();

    match result_route(&request, &outcome) {
        ResultRoute::ProgressCancel => {
            let body = DialogPresenterProgressCancelRequestV1 {
                lease,
                handle,
                attempt_token,
            };
            if issue_rpc(
                bridge,
                ids,
                state,
                PendingRpc::ProgressCancel { key },
                VERB_PRESENTER_PROGRESS_CANCEL,
                &body,
            ) {
                if let Some(active) = state.by_ui.get_mut(&key) {
                    active.phase = PresentationPhase::ProgressCancelPending;
                }
            }
            return;
        }
        ResultRoute::IgnoreProgressCompletion => return,
        ResultRoute::ResolveOrFail => {}
    }

    let mapped = catch_unwind(AssertUnwindSafe(|| mapping::egress(&request, outcome)));
    match mapped {
        Ok(Ok(value)) => {
            let body = DialogPresenterResolveRequestV1 {
                lease,
                handle,
                attempt_token,
                value,
            };
            let queued = issue_rpc(
                bridge,
                ids,
                state,
                PendingRpc::Resolve { key },
                VERB_PRESENTER_RESOLVE,
                &body,
            );
            if queued {
                if let Some(active) = state.by_ui.get_mut(&key) {
                    active.phase = PresentationPhase::TerminalPending;
                }
            }
        }
        Ok(Err(error)) => {
            warn!(%handle, "cannot map CTK result: {error}");
            issue_tracked_fail(bridge, ids, state, key, lease, handle, attempt_token);
        }
        Err(_) => {
            warn!(%handle, "dialog egress mapping panicked");
            issue_tracked_fail(bridge, ids, state, key, lease, handle, attempt_token);
        }
    }
}

fn issue_tracked_fail(
    bridge: &BusBridge,
    ids: &mut RequestIds,
    state: &mut PresenterState,
    key: UiKey,
    lease: DialogPresenterLeaseV1,
    handle: String,
    attempt_token: u64,
) {
    let body = DialogPresenterFailRequestV1 {
        lease,
        handle,
        attempt_token,
    };
    let queued = issue_rpc(
        bridge,
        ids,
        state,
        PendingRpc::Fail { key: Some(key) },
        VERB_PRESENTER_FAIL,
        &body,
    );
    if queued {
        if let Some(active) = state.by_ui.get_mut(&key) {
            active.phase = PresentationPhase::TerminalPending;
        }
    }
}

fn has_terminal_rpc(state: &PresenterState, key: UiKey) -> bool {
    state.pending_rpc.values().any(|pending| {
        matches!(
            pending,
            PendingRpc::Resolve { key: pending_key }
                | PendingRpc::ProgressCancel { key: pending_key }
                | PendingRpc::Mark { key: pending_key }
                if *pending_key == key
        ) || matches!(pending, PendingRpc::Fail { key: Some(pending_key) } if *pending_key == key)
    })
}

fn gc_cancelled_progress(time: Res<Time>, mut state: ResMut<PresenterState>) {
    gc_cancelled_progress_at(time.elapsed(), &mut state);
}

fn gc_cancelled_progress_at(now: Duration, state: &mut PresenterState) {
    let expired: Vec<_> = state
        .by_ui
        .iter()
        .filter_map(|(key, active)| {
            active
                .cancel_gc_deadline
                .is_some_and(|deadline| now >= deadline)
                .then_some(*key)
        })
        .collect();
    for key in expired {
        if let Some(active) = state.remove_key(key) {
            warn!(
                handle = %active.handle,
                "progress cancel received no owner terminal state; dropping local correlation"
            );
        }
    }
}

fn backstop_wake(
    time: Res<Time>,
    mut timer: ResMut<BackstopTimer>,
    mut state: ResMut<PresenterState>,
) {
    if timer.0.tick(time.delta()).just_finished() {
        state.drain_requested = true;
        // Also reconcile authoritative state on the backstop tick. A surface
        // born after the last reseed's watermark that later becomes absent
        // without a gap/drop/reconnect/terminal event would otherwise never be
        // pruned (post-watermark surfaces are skipped in apply_props_snapshot);
        // the backstop reseed captures a fresh, higher watermark and retires it.
        state.request_reseed();
    }
}

fn on_app_exit(
    mut exits: MessageReader<AppExit>,
    bridge: Res<BusBridge>,
    mut state: ResMut<PresenterState>,
    mut ids: ResMut<RequestIds>,
) {
    if exits.read().next().is_none() {
        return;
    }
    let Some(lease) = state.lease.clone() else {
        return;
    };
    // Best effort is sufficient here: a fresh registration always mints a
    // generation, requeues the dead presenter's active attempts, and the
    // interactd instance epoch fences every pre-restart lease.
    issue_rpc(
        &bridge,
        &mut ids,
        &mut state,
        PendingRpc::Release,
        VERB_PRESENTER_RELEASE,
        &DialogPresenterReleaseRequestV1 { lease },
    );
}

fn issue_rpc<T: Serialize>(
    bridge: &BusBridge,
    ids: &mut RequestIds,
    state: &mut PresenterState,
    pending: PendingRpc,
    command: &'static str,
    request: &T,
) -> bool {
    let body = match serde_json::to_string(request) {
        Ok(body) => body,
        Err(error) => {
            error!("cannot serialize {command}: {error}");
            return false;
        }
    };
    issue_raw_rpc(bridge, ids, state, pending, command, body)
}

fn issue_raw_rpc(
    bridge: &BusBridge,
    ids: &mut RequestIds,
    state: &mut PresenterState,
    pending: PendingRpc,
    command: &'static str,
    body: String,
) -> bool {
    let Some(request_id) = ids.bus() else {
        error!("interact-gui Bus request id counter exhausted");
        return false;
    };
    match bridge.try_call(request_id, INTERACT_SERVICE, command, BTreeMap::new(), body) {
        Ok(()) => {
            state.pending_rpc.insert(request_id, pending);
            true
        }
        Err(message) => {
            warn!("cannot queue {command}: {message}");
            false
        }
    }
}

fn handle_props_message(
    message: &BusMessage,
    state: &mut PresenterState,
    progress_updates: &mut MessageWriter<ProgressUpdate>,
    retirements: &mut Vec<SurfaceRetirement>,
) {
    let Ok(body) = serde_json::from_str::<Value>(&message.body) else {
        state.request_reseed();
        return;
    };
    if body.get("gap").and_then(Value::as_bool) == Some(true)
        || body.get("resync").and_then(Value::as_bool) == Some(true)
    {
        state.request_reseed();
        return;
    }
    let Some(path) = body.get("path").and_then(Value::as_str) else {
        state.request_reseed();
        return;
    };
    let Some((handle, leaf)) = dialog_path(path) else {
        return;
    };
    match leaf {
        "progress_fraction" => {
            if let Some(fraction) = body.get("new").and_then(Value::as_f64) {
                update_progress(handle, fraction, state, progress_updates);
            }
        }
        "state" => {
            if let Some(value) = body.get("new").and_then(Value::as_str) {
                retire_terminal(handle, value, state, retirements);
            }
        }
        _ => {}
    }
}

fn apply_props_snapshot(
    snapshot: &Value,
    watermark: u64,
    state: &mut PresenterState,
    progress_updates: &mut MessageWriter<ProgressUpdate>,
    retirements: &mut Vec<SurfaceRetirement>,
) -> bool {
    let Some(snapshot) = snapshot.as_object() else {
        return false;
    };
    let empty = serde_json::Map::new();
    let dialogs = snapshot
        .get("dialogs")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let mut progress = Vec::new();
    for (handle, record) in dialogs {
        if let Some(fraction) = record.get("progress_fraction").and_then(Value::as_f64) {
            progress.push((handle.clone(), fraction));
        }
    }
    for (handle, fraction) in progress {
        update_progress(&handle, fraction, state, progress_updates);
    }

    let local: Vec<_> = state
        .by_handle
        .iter()
        .map(|(handle, key)| (handle.clone(), *key))
        .collect();
    for (handle, key) in local {
        let Some(record) = dialogs.get(&handle) else {
            let created_after_reseed = state
                .by_ui
                .get(&key)
                .is_some_and(|active| active.created_seq > watermark);
            if created_after_reseed {
                continue;
            }
            retire_presentation(
                key,
                Some(ProgressCompletion::Failed(
                    "Progress no longer exists in interactd".into(),
                )),
                state,
                retirements,
            );
            continue;
        };
        if let Some(status) = record.get("state").and_then(Value::as_str) {
            if let Some(completion) = terminal_completion(status) {
                retire_presentation(key, Some(completion), state, retirements);
            }
        }
    }
    true
}

fn update_progress(
    handle: &str,
    fraction: f64,
    state: &PresenterState,
    updates: &mut MessageWriter<ProgressUpdate>,
) {
    if !fraction.is_finite() {
        return;
    }
    let Some(key) = state.by_handle.get(handle).copied() else {
        return;
    };
    let UiKey::Interaction(id) = key else {
        return;
    };
    let Some(active) = state.by_ui.get(&key) else {
        return;
    };
    if !is_progress_request(&active.original_request) {
        return;
    }
    let current = (fraction.clamp(0.0, 1.0) * PROGRESS_RATIO_TOTAL as f64).round() as u64;
    updates.write(
        ProgressUpdate::new(id).progress(ProgressValue::Determinate {
            current,
            total: PROGRESS_RATIO_TOTAL,
        }),
    );
}

fn retire_terminal(
    handle: &str,
    status: &str,
    state: &mut PresenterState,
    retirements: &mut Vec<SurfaceRetirement>,
) {
    let Some(completion) = terminal_completion(status) else {
        return;
    };
    let Some(key) = state.by_handle.get(handle).copied() else {
        return;
    };
    retire_presentation(key, Some(completion), state, retirements);
}

fn retire_presentation(
    key: UiKey,
    progress_completion: Option<ProgressCompletion>,
    state: &mut PresenterState,
    retirements: &mut Vec<SurfaceRetirement>,
) -> bool {
    let Some(active) = state.by_ui.get(&key) else {
        return false;
    };
    if is_progress_request(&active.original_request) {
        let UiKey::Interaction(id) = key else {
            return false;
        };
        retirements.push(SurfaceRetirement::Progress(
            id,
            progress_completion.unwrap_or_else(|| {
                ProgressCompletion::Failed("Progress presentation retired".into())
            }),
        ));
    } else {
        match key {
            UiKey::Interaction(id) => {
                retirements.push(SurfaceRetirement::Interaction(id));
            }
            UiKey::File(id) => {
                retirements.push(SurfaceRetirement::File(id));
            }
        }
    }
    state.remove_key(key);
    true
}

fn flush_surface_retirements(
    retirements: Vec<SurfaceRetirement>,
    progress_completions: &mut MessageWriter<ProgressComplete>,
    interaction_withdrawals: &mut MessageWriter<WithdrawInteraction>,
    file_withdrawals: &mut MessageWriter<WithdrawFileRequest>,
) {
    for retirement in retirements {
        match retirement {
            SurfaceRetirement::Interaction(id) => {
                interaction_withdrawals.write(WithdrawInteraction(id));
            }
            SurfaceRetirement::File(id) => {
                file_withdrawals.write(WithdrawFileRequest(id));
            }
            SurfaceRetirement::Progress(id, completion) => {
                progress_completions.write(ProgressComplete::new(id, completion));
            }
        }
    }
}

fn terminal_completion(status: &str) -> Option<ProgressCompletion> {
    match status {
        "resolved" => Some(ProgressCompletion::Succeeded),
        "cancelled" => Some(ProgressCompletion::Cancelled),
        "expired" => Some(ProgressCompletion::Failed("Progress expired".into())),
        "failed" => Some(ProgressCompletion::Failed("Progress failed".into())),
        _ => None,
    }
}

fn dialog_path(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("dialogs.")?;
    rest.rsplit_once('.')
}

fn reply_error_id(body: &str) -> Option<String> {
    let body: Value = serde_json::from_str(body).ok()?;
    body.get("error")?.get("id")?.as_str().map(str::to_owned)
}

fn is_progress_request(request: &DialogRequestV1) -> bool {
    matches!(request, DialogRequestV1::Progress { .. })
}

fn is_progress_cancel(outcome: &UiOutcome) -> bool {
    matches!(
        outcome,
        UiOutcome::Interaction(InteractionOutcome::Cancelled)
            | UiOutcome::Interaction(InteractionOutcome::Resolved(InteractionValue::Progress(
                ProgressCompletion::Cancelled
            )))
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResultRoute {
    ResolveOrFail,
    ProgressCancel,
    IgnoreProgressCompletion,
}

fn result_route(request: &DialogRequestV1, outcome: &UiOutcome) -> ResultRoute {
    if !is_progress_request(request) {
        ResultRoute::ResolveOrFail
    } else if is_progress_cancel(outcome) {
        ResultRoute::ProgressCancel
    } else {
        ResultRoute::IgnoreProgressCompletion
    }
}

fn copy_interaction_outcome(outcome: &InteractionOutcome) -> InteractionOutcome {
    match outcome {
        InteractionOutcome::Resolved(value) => {
            InteractionOutcome::Resolved(copy_interaction_value(value))
        }
        InteractionOutcome::Cancelled => InteractionOutcome::Cancelled,
        InteractionOutcome::Action(action) => InteractionOutcome::Action(action.clone()),
        InteractionOutcome::Dismissed => InteractionOutcome::Dismissed,
        _ => InteractionOutcome::Dismissed,
    }
}

fn copy_ui_outcome(outcome: &UiOutcome) -> UiOutcome {
    match outcome {
        UiOutcome::Interaction(outcome) => {
            UiOutcome::Interaction(copy_interaction_outcome(outcome))
        }
        UiOutcome::File(outcome) => UiOutcome::File(outcome.clone()),
    }
}

fn copy_interaction_value(value: &InteractionValue) -> InteractionValue {
    match value {
        InteractionValue::Acknowledged => InteractionValue::Acknowledged,
        InteractionValue::Action(action) => InteractionValue::Action(action.clone()),
        InteractionValue::Text(text) => InteractionValue::Text(text.clone()),
        InteractionValue::Secret(_) => {
            // dialog.v1 has no secret kind; turn this unreachable value into the
            // ordinary unmappable outcome which presenter_result_drive fails.
            InteractionValue::Action("__unsupported-secret-result".into())
        }
        InteractionValue::Choice(key) => InteractionValue::Choice(key.clone()),
        InteractionValue::MultiChoice(keys) => InteractionValue::MultiChoice(keys.clone()),
        InteractionValue::Progress(completion) => InteractionValue::Progress(completion.clone()),
        InteractionValue::Slider(value) => InteractionValue::Slider(*value),
        _ => InteractionValue::Action("__unsupported-future-result".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::ecs::message::Messages;
    use bevy::ecs::system::SystemState;
    use bevy::prelude::World;
    use cosmix_interaction_schema::{DialogCommonV1, DialogProgressValueV1, DialogSeverityV1};
    use serde_json::json;

    fn common() -> DialogCommonV1 {
        DialogCommonV1 {
            title: "Test".into(),
            message: Some("Message".into()),
            severity: DialogSeverityV1::Info,
        }
    }

    fn message_request() -> DialogRequestV1 {
        DialogRequestV1::Message {
            common: common(),
            details: None,
        }
    }

    fn progress_request() -> DialogRequestV1 {
        DialogRequestV1::Progress {
            common: common(),
            progress: DialogProgressValueV1::Indeterminate {},
            cancellable: true,
        }
    }

    fn file_request() -> DialogRequestV1 {
        DialogRequestV1::FileOpen {
            common: common(),
            initial_directory: None,
            filters: vec![],
            multiple: false,
        }
    }

    fn presentation(handle: &str, attempt_token: u64) -> DialogPresentationV1 {
        DialogPresentationV1 {
            handle: handle.into(),
            attempt_token,
            dialog: message_request(),
            progress: None,
            cancel_requested: false,
        }
    }

    fn insert_active(
        state: &mut PresenterState,
        request: DialogRequestV1,
        phase: PresentationPhase,
    ) -> UiKey {
        let key = UiKey::Interaction(InteractionId::next());
        insert_active_as(state, "dialog-1", key, request, phase);
        key
    }

    fn insert_active_as(
        state: &mut PresenterState,
        handle: &str,
        key: UiKey,
        request: DialogRequestV1,
        phase: PresentationPhase,
    ) {
        let created_seq = state.next_surface_seq();
        let active = ActivePresentation {
            handle: handle.into(),
            attempt_token: 7,
            original_request: request,
            phase,
            pending_outcome: None,
            created_seq,
            needs_rebind: false,
            cancel_gc_deadline: None,
        };
        assert!(state.insert_correlation(key, active), "unique test key");
    }

    #[test]
    fn correlation_is_inserted_before_mark_and_removed_after_terminal_reply() {
        let mut state = PresenterState::default();
        let key = insert_active(
            &mut state,
            message_request(),
            PresentationPhase::AwaitingMark,
        );
        assert_eq!(state.by_handle.get("dialog-1"), Some(&key));
        assert_eq!(
            state.by_ui.get(&key).map(|active| active.phase),
            Some(PresentationPhase::AwaitingMark)
        );

        state.by_ui.get_mut(&key).expect("active").phase = PresentationPhase::TerminalPending;
        let removed = state.remove_key(key).expect("terminal correlation");
        assert_eq!(removed.handle, "dialog-1");
        assert!(!state.by_ui.contains_key(&key));
        assert!(!state.by_handle.contains_key("dialog-1"));
    }

    #[test]
    fn duplicate_handle_is_deduplicated_and_new_attempt_is_adopted() {
        let mut state = PresenterState::default();
        let key = insert_active(&mut state, message_request(), PresentationPhase::Showing);

        assert!(state.reconcile_existing(&presentation("dialog-1", 7)));
        assert_eq!(state.by_ui.len(), 1);
        assert_eq!(state.by_handle.get("dialog-1"), Some(&key));
        assert_eq!(state.by_ui[&key].phase, PresentationPhase::Showing);

        assert!(state.reconcile_existing(&presentation("dialog-1", 8)));
        assert_eq!(state.by_ui.len(), 1);
        assert_eq!(state.by_ui[&key].attempt_token, 8);
        assert_eq!(state.by_ui[&key].phase, PresentationPhase::AwaitingMark);
    }

    #[test]
    fn outcome_before_mark_reply_is_buffered() {
        let mut state = PresenterState::default();
        let key = insert_active(
            &mut state,
            message_request(),
            PresentationPhase::AwaitingMark,
        );
        state.store_outcome(
            key,
            UiOutcome::Interaction(InteractionOutcome::Resolved(InteractionValue::Acknowledged)),
        );

        let active = &state.by_ui[&key];
        assert_eq!(active.phase, PresentationPhase::AwaitingMark);
        assert!(active.pending_outcome.is_some());

        state.mark_acknowledged(key);
        assert_eq!(state.by_ui[&key].phase, PresentationPhase::Showing);
        assert!(state.by_ui[&key].pending_outcome.is_some());
    }

    #[test]
    fn transport_generation_drop_retries_a_buffered_terminal_result() {
        let mut state = PresenterState::default();
        let key = insert_active(
            &mut state,
            message_request(),
            PresentationPhase::TerminalPending,
        );
        state.by_ui.get_mut(&key).expect("active").pending_outcome = Some(UiOutcome::Interaction(
            InteractionOutcome::Resolved(InteractionValue::Acknowledged),
        ));
        state.pending_rpc.insert(99, PendingRpc::Resolve { key });

        state.drop_transport_inflight();

        assert!(state.pending_rpc.is_empty());
        assert_eq!(state.by_ui[&key].phase, PresentationPhase::Showing);
        assert!(state.by_ui[&key].pending_outcome.is_some());
    }

    #[test]
    fn progress_is_never_routed_to_presenter_resolve() {
        let request = progress_request();
        assert_eq!(
            result_route(
                &request,
                &UiOutcome::Interaction(InteractionOutcome::Resolved(InteractionValue::Progress(
                    ProgressCompletion::Succeeded
                ),)),
            ),
            ResultRoute::IgnoreProgressCompletion
        );
        assert_eq!(
            result_route(
                &request,
                &UiOutcome::Interaction(InteractionOutcome::Resolved(InteractionValue::Progress(
                    ProgressCompletion::Cancelled
                ),)),
            ),
            ResultRoute::ProgressCancel
        );
        assert_ne!(
            result_route(
                &request,
                &UiOutcome::Interaction(InteractionOutcome::Cancelled),
            ),
            ResultRoute::ResolveOrFail
        );
    }

    #[test]
    fn stale_lease_preserves_correlations_for_broker_re_adoption() {
        let mut state = PresenterState::default();
        let key = insert_active(&mut state, message_request(), PresentationPhase::Showing);
        state.lease = Some(DialogPresenterLeaseV1 {
            presenter_service: "interact-gui".into(),
            generation: 1,
            instance_epoch: 2,
        });
        state.invalidate_lease();

        assert!(state.lease.is_none());
        assert!(state.by_ui[&key].needs_rebind);
        assert_eq!(state.by_ui[&key].phase, PresentationPhase::AwaitingMark);
    }

    #[test]
    fn terminal_state_retires_modal_file_and_progress_surfaces() {
        let mut state = PresenterState::default();
        let modal = insert_active(&mut state, message_request(), PresentationPhase::Showing);
        let UiKey::Interaction(modal_id) = modal else {
            unreachable!();
        };
        let file_id = FileRequestId(41);
        insert_active_as(
            &mut state,
            "dialog-file",
            UiKey::File(file_id),
            file_request(),
            PresentationPhase::Showing,
        );
        let progress_id = InteractionId::next();
        insert_active_as(
            &mut state,
            "dialog-progress",
            UiKey::Interaction(progress_id),
            progress_request(),
            PresentationPhase::Showing,
        );
        let mut retirements = Vec::new();

        retire_terminal("dialog-1", "resolved", &mut state, &mut retirements);
        retire_terminal("dialog-file", "failed", &mut state, &mut retirements);
        retire_terminal("dialog-progress", "cancelled", &mut state, &mut retirements);

        assert_eq!(
            retirements,
            [
                SurfaceRetirement::Interaction(modal_id),
                SurfaceRetirement::File(file_id),
                SurfaceRetirement::Progress(progress_id, ProgressCompletion::Cancelled),
            ]
        );
        assert!(state.by_ui.is_empty());
        assert!(state.by_handle.is_empty());
    }

    #[test]
    fn terminal_mark_error_withdraws_before_dropping_correlation() {
        let mut state = PresenterState::default();
        let key = insert_active(
            &mut state,
            message_request(),
            PresentationPhase::AwaitingMark,
        );
        let UiKey::Interaction(id) = key else {
            unreachable!();
        };
        let mut retirements = Vec::new();

        handle_application_error(
            PendingRpc::Mark { key },
            Some("stale_attempt"),
            &mut state,
            &mut retirements,
        );

        assert_eq!(retirements, [SurfaceRetirement::Interaction(id)]);
        assert!(!state.by_ui.contains_key(&key));
        assert!(!state.by_handle.contains_key("dialog-1"));
    }

    #[test]
    fn full_snapshot_prunes_absent_and_terminal_correlations_but_keeps_live() {
        let mut state = PresenterState::default();
        let absent_id = InteractionId::next();
        let terminal_id = InteractionId::next();
        let live_id = InteractionId::next();
        insert_active_as(
            &mut state,
            "absent",
            UiKey::Interaction(absent_id),
            message_request(),
            PresentationPhase::AwaitingMark,
        );
        insert_active_as(
            &mut state,
            "terminal",
            UiKey::Interaction(terminal_id),
            message_request(),
            PresentationPhase::AwaitingMark,
        );
        insert_active_as(
            &mut state,
            "live",
            UiKey::Interaction(live_id),
            message_request(),
            PresentationPhase::AwaitingMark,
        );
        let snapshot = json!({
            "dialogs": {
                "terminal": {"state": "expired"},
                "live": {"state": "presented"}
            }
        });
        let mut retirements = Vec::new();
        let mut world = World::new();
        world.init_resource::<Messages<ProgressUpdate>>();
        let mut writer_state = SystemState::<MessageWriter<ProgressUpdate>>::new(&mut world);
        let applied = {
            let mut updates = writer_state
                .get_mut(&mut world)
                .expect("progress writer is available");
            apply_props_snapshot(
                &snapshot,
                state.surface_seq,
                &mut state,
                &mut updates,
                &mut retirements,
            )
        };
        writer_state.apply(&mut world);

        assert!(applied);
        assert_eq!(retirements.len(), 2);
        assert!(retirements.contains(&SurfaceRetirement::Interaction(absent_id)));
        assert!(retirements.contains(&SurfaceRetirement::Interaction(terminal_id)));
        assert_eq!(state.by_ui.len(), 1);
        assert_eq!(
            state.by_handle.get("live"),
            Some(&UiKey::Interaction(live_id))
        );
    }

    #[test]
    fn stale_snapshot_prunes_only_surfaces_at_or_before_its_issue_watermark() {
        let mut state = PresenterState::default();
        let stale_id = InteractionId::next();
        insert_active_as(
            &mut state,
            "stale",
            UiKey::Interaction(stale_id),
            message_request(),
            PresentationPhase::Showing,
        );
        let watermark = state.surface_seq;

        let fresh_id = InteractionId::next();
        insert_active_as(
            &mut state,
            "fresh",
            UiKey::Interaction(fresh_id),
            message_request(),
            PresentationPhase::Showing,
        );
        assert!(state.by_ui[&UiKey::Interaction(fresh_id)].created_seq > watermark);

        let mut world = World::new();
        world.init_resource::<Messages<ProgressUpdate>>();
        world.init_resource::<Messages<ProgressComplete>>();
        world.init_resource::<Messages<WithdrawInteraction>>();
        world.init_resource::<Messages<WithdrawFileRequest>>();
        let mut progress_writer_state =
            SystemState::<MessageWriter<ProgressUpdate>>::new(&mut world);
        let mut retirements = Vec::new();
        let applied = {
            let mut updates = progress_writer_state
                .get_mut(&mut world)
                .expect("progress writer is available");
            apply_props_snapshot(
                &json!({"dialogs": {}}),
                watermark,
                &mut state,
                &mut updates,
                &mut retirements,
            )
        };
        progress_writer_state.apply(&mut world);

        assert!(applied);
        assert_eq!(retirements, [SurfaceRetirement::Interaction(stale_id)]);
        assert!(!state.by_ui.contains_key(&UiKey::Interaction(stale_id)));
        assert!(!state.by_handle.contains_key("stale"));
        assert!(state.by_ui.contains_key(&UiKey::Interaction(fresh_id)));
        assert_eq!(
            state.by_handle.get("fresh"),
            Some(&UiKey::Interaction(fresh_id))
        );

        let mut retirement_writer_state = SystemState::<(
            MessageWriter<ProgressComplete>,
            MessageWriter<WithdrawInteraction>,
            MessageWriter<WithdrawFileRequest>,
        )>::new(&mut world);
        {
            let (mut completions, mut interactions, mut files) = retirement_writer_state
                .get_mut(&mut world)
                .expect("retirement writers are available");
            flush_surface_retirements(retirements, &mut completions, &mut interactions, &mut files);
        }
        retirement_writer_state.apply(&mut world);

        let withdrawals = world.resource::<Messages<WithdrawInteraction>>();
        let mut cursor = withdrawals.get_cursor();
        let withdrawals: Vec<_> = cursor.read(withdrawals).collect();
        assert_eq!(withdrawals, [&WithdrawInteraction(stale_id)]);
        assert!(!withdrawals.contains(&&WithdrawInteraction(fresh_id)));
    }

    #[test]
    fn sparse_object_reseed_is_authoritative_empty_and_completes() {
        let mut state = PresenterState {
            reseed_in_flight: true,
            reseed_failures: 2,
            ..PresenterState::default()
        };
        let interaction_id = InteractionId::next();
        let progress_id = InteractionId::next();
        insert_active_as(
            &mut state,
            "dialog-interaction",
            UiKey::Interaction(interaction_id),
            message_request(),
            PresentationPhase::Showing,
        );
        insert_active_as(
            &mut state,
            "dialog-progress",
            UiKey::Interaction(progress_id),
            progress_request(),
            PresentationPhase::Showing,
        );
        state.reseed_inflight_watermark = state.surface_seq;

        let mut world = World::new();
        world.init_resource::<Messages<ProgressUpdate>>();
        world.init_resource::<Messages<ProgressComplete>>();
        world.init_resource::<Messages<WithdrawInteraction>>();
        world.init_resource::<Messages<WithdrawFileRequest>>();
        let mut progress_writer_state =
            SystemState::<MessageWriter<ProgressUpdate>>::new(&mut world);
        let mut retirements = Vec::new();
        let applied = {
            let mut updates = progress_writer_state
                .get_mut(&mut world)
                .expect("progress writer is available");
            apply_props_snapshot(
                &json!({"lifecycle": {"props_level": "L2"}}),
                state.reseed_inflight_watermark,
                &mut state,
                &mut updates,
                &mut retirements,
            )
        };
        progress_writer_state.apply(&mut world);
        assert!(applied);
        state.complete_reseed();

        assert!(state.by_ui.is_empty());
        assert!(state.by_handle.is_empty());
        assert!(!state.reseed_requested);
        assert!(!state.reseed_in_flight);
        assert_eq!(state.reseed_failures, 0);

        let mut retirement_writer_state = SystemState::<(
            MessageWriter<ProgressComplete>,
            MessageWriter<WithdrawInteraction>,
            MessageWriter<WithdrawFileRequest>,
        )>::new(&mut world);
        {
            let (mut completions, mut interactions, mut files) = retirement_writer_state
                .get_mut(&mut world)
                .expect("retirement writers are available");
            flush_surface_retirements(retirements, &mut completions, &mut interactions, &mut files);
        }
        retirement_writer_state.apply(&mut world);

        let withdrawals = world.resource::<Messages<WithdrawInteraction>>();
        let mut withdrawal_cursor = withdrawals.get_cursor();
        let withdrawals: Vec<_> = withdrawal_cursor.read(withdrawals).collect();
        assert_eq!(withdrawals, [&WithdrawInteraction(interaction_id)]);

        let completions = world.resource::<Messages<ProgressComplete>>();
        let mut completion_cursor = completions.get_cursor();
        let completions: Vec<_> = completion_cursor.read(completions).collect();
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].id, progress_id);
        assert_eq!(
            completions[0].completion,
            ProgressCompletion::Failed("Progress no longer exists in interactd".into())
        );
    }

    #[test]
    fn progress_cancel_ack_is_locally_garbage_collected_after_bound() {
        let mut state = PresenterState::default();
        let key = insert_active(
            &mut state,
            progress_request(),
            PresentationPhase::ProgressCancelPending,
        );
        state.by_ui.get_mut(&key).expect("active").pending_outcome =
            Some(UiOutcome::Interaction(InteractionOutcome::Cancelled));
        let now = Duration::from_secs(10);

        acknowledge_progress_cancel(key, now, &mut state);

        assert_eq!(
            state.by_ui[&key].phase,
            PresentationPhase::ProgressCancelAwaitingOwner
        );
        assert!(state.by_ui[&key].pending_outcome.is_none());
        let deadline = now + PROGRESS_CANCEL_RETENTION;
        assert_eq!(state.by_ui[&key].cancel_gc_deadline, Some(deadline));
        gc_cancelled_progress_at(deadline - Duration::from_millis(1), &mut state);
        assert!(state.by_ui.contains_key(&key));
        gc_cancelled_progress_at(deadline, &mut state);
        assert!(!state.by_ui.contains_key(&key));
        assert!(
            state.pending_rpc.is_empty(),
            "GC must not issue a wire call"
        );
    }

    #[test]
    fn every_reseed_failure_rearms_with_bounded_backoff() {
        let now = Duration::from_secs(20);
        let failures = [
            Err("transport".into()),
            Ok(BusReply {
                rc: 1,
                body: json!({"error": {"id": "temporary"}}).to_string(),
                result: None,
            }),
            Ok(BusReply {
                rc: 0,
                body: "{broken".into(),
                result: None,
            }),
            Ok(BusReply {
                rc: 0,
                body: json!([]).to_string(),
                result: None,
            }),
        ];
        let mut world = World::new();
        world.init_resource::<Messages<ProgressUpdate>>();
        let mut writer_state = SystemState::<MessageWriter<ProgressUpdate>>::new(&mut world);

        for failure in failures {
            let mut state = PresenterState {
                reseed_in_flight: true,
                ..PresenterState::default()
            };
            let mut retirements = Vec::new();
            {
                let mut updates = writer_state
                    .get_mut(&mut world)
                    .expect("progress writer is available");
                handle_rpc_reply(
                    PendingRpc::Reseed,
                    failure,
                    &mut state,
                    &mut updates,
                    &mut retirements,
                    now,
                );
            }
            writer_state.apply(&mut world);
            assert!(state.reseed_requested);
            assert!(!state.reseed_in_flight);
            assert!(state.reseed_not_before > now);
            assert!(state.reseed_not_before <= now + RESEED_RETRY_MAX);
        }

        let mut state = PresenterState::default();
        for _ in 0..32 {
            state.schedule_reseed_retry(now);
            assert!(state.reseed_not_before <= now + RESEED_RETRY_MAX);
        }
    }

    #[test]
    fn valid_reseed_is_complete_only_after_snapshot_application() {
        let mut state = PresenterState {
            reseed_requested: true,
            reseed_in_flight: true,
            reseed_failures: 3,
            ..PresenterState::default()
        };
        let mut world = World::new();
        world.init_resource::<Messages<ProgressUpdate>>();
        let mut writer_state = SystemState::<MessageWriter<ProgressUpdate>>::new(&mut world);
        let mut retirements = Vec::new();
        {
            let mut updates = writer_state
                .get_mut(&mut world)
                .expect("progress writer is available");
            handle_rpc_reply(
                PendingRpc::Reseed,
                Ok(BusReply {
                    rc: 0,
                    body: json!({"dialogs": {}}).to_string(),
                    result: None,
                }),
                &mut state,
                &mut updates,
                &mut retirements,
                Duration::from_secs(1),
            );
        }
        writer_state.apply(&mut world);

        assert!(!state.reseed_requested);
        assert!(!state.reseed_in_flight);
        assert_eq!(state.reseed_failures, 0);
        assert_eq!(state.reseed_not_before, Duration::ZERO);
    }

    #[test]
    fn successful_reseed_preserves_newer_gap_intent() {
        let mut state = PresenterState {
            reseed_in_flight: true,
            ..PresenterState::default()
        };
        let mut world = World::new();
        world.init_resource::<Messages<ProgressUpdate>>();
        let mut writer_state = SystemState::<MessageWriter<ProgressUpdate>>::new(&mut world);
        let mut retirements = Vec::new();

        let gap = BusMessage {
            connection_generation: 1,
            from: "interact".into(),
            command: "props.changed".into(),
            body: json!({"gap": true}).to_string(),
            headers: BTreeMap::new(),
        };
        {
            let mut updates = writer_state
                .get_mut(&mut world)
                .expect("progress writer is available");
            handle_props_message(&gap, &mut state, &mut updates, &mut retirements);
        }
        writer_state.apply(&mut world);
        assert!(state.reseed_requested);
        assert!(state.reseed_dirty);

        {
            let mut updates = writer_state
                .get_mut(&mut world)
                .expect("progress writer is available");
            handle_rpc_reply(
                PendingRpc::Reseed,
                Ok(BusReply {
                    rc: 0,
                    body: json!({"dialogs": {}}).to_string(),
                    result: None,
                }),
                &mut state,
                &mut updates,
                &mut retirements,
                Duration::from_secs(1),
            );
        }
        writer_state.apply(&mut world);

        assert!(state.reseed_requested);
        assert!(!state.reseed_in_flight);
        assert!(!state.reseed_dirty);
        assert_eq!(state.reseed_failures, 0);
        assert_eq!(state.reseed_not_before, Duration::ZERO);
    }

    #[test]
    fn inconsistent_reverse_correlation_is_repaired_without_panicking() {
        let mut state = PresenterState::default();
        let key = UiKey::Interaction(InteractionId::next());
        state.by_handle.insert("dialog-1".into(), key);

        assert!(!state.reconcile_existing(&presentation("dialog-1", 8)));
        assert!(!state.by_handle.contains_key("dialog-1"));
        assert!(!state.by_ui.contains_key(&key));
    }

    #[test]
    fn occupied_forward_correlation_rejects_new_presentation() {
        let mut state = PresenterState::default();
        let key = insert_active(&mut state, message_request(), PresentationPhase::Showing);
        let created_seq = state.next_surface_seq();
        let rejected = ActivePresentation {
            handle: "dialog-2".into(),
            attempt_token: 8,
            original_request: message_request(),
            phase: PresentationPhase::AwaitingMark,
            pending_outcome: None,
            created_seq,
            needs_rebind: false,
            cancel_gc_deadline: None,
        };

        assert!(!state.insert_correlation(key, rejected));
        assert_eq!(state.by_ui[&key].handle, "dialog-1");
        assert_eq!(state.by_handle.get("dialog-1"), Some(&key));
        assert!(!state.by_handle.contains_key("dialog-2"));
    }
}
