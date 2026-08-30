//! Event-driven, bounded Bus fan-out for Tower's atlas model.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use bevy::app::{App, AppExit, Plugin, Update};
use bevy::ecs::message::{Message, MessageReader};
use bevy::prelude::{IntoScheduleConfigs, Res, ResMut, Resource};
use ctk::prelude::{BusBridge, BusBridgeEvent, BusConnectionState, BusMessage, BusReply};
use serde_json::Value;

use crate::bus::{retained_service, INTERACT_CHANGED};
use crate::config::SavedFilters;
use crate::inspector::{
    parse_action_description, parse_actions_list, parse_control_value, parse_controls_list,
    truncate_text, AppDescription, CitizenInspector, InspectCitizen, InspectorMutation,
    InspectorMutationRequest, InspectorResult, MutationTarget, ProcessIdentity,
};
use crate::model::{
    now_unix_ms, parse_service_list, AtlasState, InventoryProjection, NodeInfo, PeersProjection,
    RefreshReason,
};
use crate::props::{
    is_flat_surface, is_namespaced_surface, parse_path_list, PropsAvailability, PropsSurface,
};
use crate::topology::TopologyActivity;
use crate::traffic::{TrafficIntent, TrafficState};

const MAX_INFLIGHT: usize = 8;
/// Remote calls may occupy at most this many in-flight slots. The remainder
/// is reserved for bootstrap/local work, so a fan-out stuck on unreachable
/// remotes' timeouts can never delay a local changed-event read.
const REMOTE_MAX_INFLIGHT: usize = 6;
const MAX_QUEUED: usize = 128;

#[derive(Message, Clone, Copy, Debug, Default)]
pub(crate) struct RefreshMesh;

pub(crate) struct AtlasPlugin;

impl Plugin for AtlasPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<AtlasState>()
            .init_resource::<AtlasRuntime>()
            .init_resource::<TrafficRuntime>()
            .init_resource::<TrafficState>()
            .init_resource::<TopologyActivity>()
            .add_message::<RefreshMesh>()
            .add_message::<InspectCitizen>()
            .add_message::<InspectorMutationRequest>()
            .add_message::<TrafficIntent>()
            .add_systems(
                Update,
                (
                    drain_bus,
                    handle_traffic_intents,
                    pump_traffic_observer,
                    handle_manual_refresh,
                    handle_inspector_requests,
                    handle_inspector_mutations,
                    purge_invalid_queued_mutations,
                    pump_requests,
                    stop_observer_on_exit,
                )
                    .chain(),
            );
    }
}

fn stop_observer_on_exit(
    mut exits: MessageReader<AppExit>,
    bridge: Option<Res<BusBridge>>,
    traffic: Res<TrafficState>,
) {
    if exits.read().next().is_none() {
        return;
    }
    let (Some(bridge), Some(subscription_id)) = (bridge, traffic.subscription_id.as_ref()) else {
        return;
    };
    let _ = bridge.try_observe_stop_flush(
        u64::MAX,
        serde_json::json!({"subscription_id": subscription_id}).to_string(),
    );
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum RequestKind {
    Inventory,
    Peers,
    NodeInfo { node: String },
    NodeList { node: String },
    PropsList { service: String },
    PropsGet { service: String },
    PropsDescribe { service: String, path: String },
    AppDescribe { service: String },
    ActionsList { service: String },
    ActionDescribe { service: String, action: String },
    ControlsList { service: String },
    ControlGet { service: String, control: String },
    ActionInvoke { service: String, action: String },
    ControlSet { service: String, control: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequestPriority {
    Bootstrap,
    Local,
    Remote,
}

#[derive(Clone, Debug)]
struct QueuedCall {
    generation: u64,
    refresh_epoch: u64,
    sequence: u64,
    priority: RequestPriority,
    to: String,
    command: String,
    headers: BTreeMap<String, String>,
    body: String,
    kind: RequestKind,
    mutation: Option<MutationGuard>,
}

#[derive(Clone, Debug)]
struct MutationGuard {
    identity: ProcessIdentity,
    target: MutationTarget,
    operation: u64,
    invalidated: bool,
}

#[derive(Resource, Debug)]
struct AtlasRuntime {
    next_request_id: u64,
    next_sequence: u64,
    next_operation_id: u64,
    refresh_epoch: u64,
    bootstrap_queue: VecDeque<RequestKind>,
    local_queue: VecDeque<RequestKind>,
    remote_queue: VecDeque<RequestKind>,
    queued: HashMap<RequestKind, QueuedCall>,
    pending: HashMap<u64, QueuedCall>,
    in_flight: HashSet<RequestKind>,
    reread_needed: HashSet<RequestKind>,
    latest_sequence: HashMap<RequestKind, u64>,
    inventory_verified: bool,
    peers_ready: bool,
    staged_peers: Option<PeersProjection>,
    fanout_scheduled: bool,
    remote_fanout_active: bool,
    remote_cursor: usize,
    remote_local: Option<String>,
    refresh_failure: Option<String>,
}

impl Default for AtlasRuntime {
    fn default() -> Self {
        Self {
            next_request_id: 1,
            next_sequence: 1,
            next_operation_id: 1,
            refresh_epoch: 0,
            bootstrap_queue: VecDeque::new(),
            local_queue: VecDeque::new(),
            remote_queue: VecDeque::new(),
            queued: HashMap::new(),
            pending: HashMap::new(),
            in_flight: HashSet::new(),
            reread_needed: HashSet::new(),
            latest_sequence: HashMap::new(),
            inventory_verified: false,
            peers_ready: false,
            staged_peers: None,
            fanout_scheduled: false,
            remote_fanout_active: false,
            remote_cursor: 0,
            remote_local: None,
            refresh_failure: None,
        }
    }
}

impl AtlasRuntime {
    fn clear_work(&mut self) {
        self.bootstrap_queue.clear();
        self.local_queue.clear();
        self.remote_queue.clear();
        self.queued.clear();
        self.pending.clear();
        self.in_flight.clear();
        self.reread_needed.clear();
        self.latest_sequence.clear();
        self.inventory_verified = false;
        self.peers_ready = false;
        self.staged_peers = None;
        self.fanout_scheduled = false;
        self.remote_fanout_active = false;
        self.remote_cursor = 0;
        self.remote_local = None;
        self.refresh_failure = None;
    }

    fn is_idle(&self) -> bool {
        self.queued.is_empty() && self.pending.is_empty() && !self.remote_fanout_active
    }

    fn has_local_work(&self) -> bool {
        !self.bootstrap_queue.is_empty()
            || !self.local_queue.is_empty()
            || self
                .pending
                .values()
                .any(|call| call.priority != RequestPriority::Remote)
    }
}

#[derive(Clone, Debug)]
enum TrafficCall {
    Start { revision: u64 },
    Stop { subscription_id: String },
}

#[derive(Clone, Debug)]
struct PendingTrafficCall {
    generation: u64,
    call: TrafficCall,
}

#[derive(Resource, Debug)]
struct TrafficRuntime {
    next_request_id: u64,
    pending: HashMap<u64, PendingTrafficCall>,
    desired_start: bool,
    desired_stop: Option<String>,
}

impl Default for TrafficRuntime {
    fn default() -> Self {
        Self {
            next_request_id: 1,
            pending: HashMap::new(),
            desired_start: false,
            desired_stop: None,
        }
    }
}

fn drain_bus(
    bridge: Option<Res<BusBridge>>,
    mut atlas: ResMut<AtlasState>,
    mut runtime: ResMut<AtlasRuntime>,
    mut traffic: ResMut<TrafficState>,
    mut traffic_runtime: ResMut<TrafficRuntime>,
    mut topology_activity: ResMut<TopologyActivity>,
) {
    let Some(bridge) = bridge else {
        return;
    };
    for event in bridge.drain_events() {
        match event {
            BusBridgeEvent::Connection { state, generation } => {
                handle_connection(state, generation, &mut atlas, &mut runtime);
            }
            BusBridgeEvent::Reply { request_id, result } => {
                handle_reply(request_id, result, &mut atlas, &mut runtime);
            }
            BusBridgeEvent::DroppedMessages(count) => {
                atlas.notice = Some(format!("local telemetry dropped {count} messages"));
                atlas.bump();
            }
            BusBridgeEvent::ObservationConnection { state, generation } => {
                handle_observation_connection(
                    state,
                    generation,
                    &mut traffic,
                    &mut traffic_runtime,
                );
            }
            BusBridgeEvent::ObservationReply { request_id, result } => {
                handle_observation_reply(request_id, result, &mut traffic, &mut traffic_runtime);
            }
            BusBridgeEvent::ObservationDroppedMessages(count) => {
                if traffic.subscription_id.is_some() {
                    traffic.record_transport_drops(count);
                }
            }
            BusBridgeEvent::Fatal(error) => {
                atlas.connection = BusConnectionState::Fatal;
                atlas.notice = Some(format!("Bus bridge stopped: {error}"));
                atlas.mark_all_remote_stale();
            }
        }
    }

    let mut messages = bridge.drain_latest_messages();
    messages.extend(bridge.drain_messages());
    for message in messages {
        handle_topic(message, &mut atlas, &mut runtime);
    }
    for message in bridge.drain_observation_messages() {
        if let Some(event) = traffic.handle_message(&message) {
            topology_activity.record(&event, &atlas);
        }
    }
    finish_refresh_if_idle(&mut atlas, &mut runtime);
}

fn handle_connection(
    state: BusConnectionState,
    generation: u64,
    atlas: &mut AtlasState,
    runtime: &mut AtlasRuntime,
) {
    atlas.connection = state;
    match state {
        BusConnectionState::Connected if atlas.connection_generation != generation => {
            let reason = if atlas.connection_generation == 0 {
                RefreshReason::Startup
            } else {
                atlas.mark_all_remote_stale();
                RefreshReason::Reconnect
            };
            atlas.connection_generation = generation;
            begin_refresh(reason, atlas, runtime);
        }
        BusConnectionState::Disconnected
        | BusConnectionState::ShuttingDown
        | BusConnectionState::Fatal => {
            runtime.clear_work();
            atlas.active_mutations.clear();
            atlas.refreshing = false;
            atlas.mark_all_remote_stale();
        }
        BusConnectionState::Connecting | BusConnectionState::Connected => {}
    }
    atlas.bump();
}

fn handle_traffic_intents(
    mut intents: MessageReader<TrafficIntent>,
    atlas: Res<AtlasState>,
    mut runtime: ResMut<TrafficRuntime>,
    mut traffic: ResMut<TrafficState>,
    mut saved: ResMut<SavedFilters>,
) {
    for intent in intents.read() {
        match intent {
            TrafficIntent::SetOpen(open) => {
                set_traffic_open(&mut runtime, &mut traffic, *open);
            }
            TrafficIntent::TogglePause => traffic.toggle_pause(),
            TrafficIntent::CycleVerb => {
                saved.clear_active();
                traffic.cycle_verb();
                restart_observer(&mut runtime, &mut traffic);
            }
            TrafficIntent::CycleService => {
                let services = atlas
                    .local_node()
                    .and_then(|node| atlas.nodes.get(node))
                    .map_or_else(Vec::new, |node| {
                        node.citizens
                            .iter()
                            .map(|service| service.name.clone())
                            .collect()
                    });
                saved.clear_active();
                traffic.cycle_service(&services);
                restart_observer(&mut runtime, &mut traffic);
            }
            TrafficIntent::CycleDirection => {
                saved.clear_active();
                traffic.cycle_direction();
                restart_observer(&mut runtime, &mut traffic);
            }
            TrafficIntent::ToggleBody => {
                saved.clear_active();
                traffic.toggle_body();
                restart_observer(&mut runtime, &mut traffic);
            }
            TrafficIntent::SaveNamed(name) => match saved.save_current(name, &traffic.filter) {
                Ok(name) => traffic.notice(format!("Saved traffic filter \"{name}\"")),
                Err(error) => traffic.notice(format!("Filter not saved: {error}")),
            },
            TrafficIntent::SelectNamed(name) => {
                if let Some(filter) = saved.select(name) {
                    traffic.apply_filter(filter);
                    traffic.notice(format!("Loaded traffic filter \"{name}\""));
                    restart_observer(&mut runtime, &mut traffic);
                } else {
                    traffic.notice(format!("Saved filter \"{name}\" no longer exists"));
                }
            }
            TrafficIntent::DeleteNamed(name) => {
                if saved.delete(name) {
                    traffic.notice(format!("Deleted traffic filter \"{name}\""));
                }
            }
            TrafficIntent::Select(seq) => traffic.select(*seq),
        }
    }
}

fn set_traffic_open(runtime: &mut TrafficRuntime, traffic: &mut TrafficState, open: bool) {
    if traffic.open == open {
        return;
    }
    traffic.set_open(open);
    if open {
        if traffic.observation_connection == BusConnectionState::Connected {
            runtime.desired_start = true;
        }
    } else if let Some(subscription_id) = traffic.subscription_id.clone() {
        runtime.desired_stop = Some(subscription_id);
        traffic.stop_pending(false);
    } else if !runtime.pending.is_empty()
        || traffic.observation_connection != BusConnectionState::Connected
    {
        let disconnected = traffic.observation_connection != BusConnectionState::Connected;
        traffic.stop_pending(disconnected);
    } else {
        // Nothing reached the broker, so there is no subscription to fence
        // and the local close is already complete.
        traffic.stop_succeeded();
    }
}

fn restart_observer(runtime: &mut TrafficRuntime, traffic: &mut TrafficState) {
    if !traffic.open || traffic.observation_connection != BusConnectionState::Connected {
        return;
    }
    if let Some(subscription_id) = traffic.subscription_id.clone() {
        runtime.desired_stop = Some(subscription_id);
        traffic.stop_pending(false);
    } else if runtime.pending.is_empty() {
        runtime.desired_start = true;
    }
}

fn pump_traffic_observer(
    bridge: Option<Res<BusBridge>>,
    mut runtime: ResMut<TrafficRuntime>,
    mut traffic: ResMut<TrafficState>,
) {
    let Some(bridge) = bridge else {
        return;
    };
    if traffic.observation_connection != BusConnectionState::Connected
        || !runtime.pending.is_empty()
    {
        return;
    }
    let (call, command, body) = if let Some(subscription_id) = runtime.desired_stop.take() {
        let body = serde_json::json!({"subscription_id": subscription_id}).to_string();
        (
            TrafficCall::Stop { subscription_id },
            "noded.observe.stop",
            body,
        )
    } else if runtime.desired_start && traffic.open && traffic.subscription_id.is_none() {
        runtime.desired_start = false;
        (
            TrafficCall::Start {
                revision: traffic.filter_revision,
            },
            "noded.observe.start",
            traffic.filter.start_body(),
        )
    } else {
        return;
    };
    let request_id = runtime.next_request_id;
    match bridge.try_observe_call(request_id, "noded", command, BTreeMap::new(), body) {
        Ok(()) => {
            runtime.next_request_id = runtime.next_request_id.wrapping_add(1);
            runtime.pending.insert(
                request_id,
                PendingTrafficCall {
                    generation: traffic.connection_generation,
                    call,
                },
            );
        }
        Err(error) => {
            match call {
                TrafficCall::Start { .. } => runtime.desired_start = true,
                TrafficCall::Stop { subscription_id } => {
                    runtime.desired_stop = Some(subscription_id);
                    traffic.stop_pending(false);
                }
            }
            traffic.request_queue_busy(error);
        }
    }
}

fn handle_manual_refresh(
    mut refresh: MessageReader<RefreshMesh>,
    mut atlas: ResMut<AtlasState>,
    mut runtime: ResMut<AtlasRuntime>,
) {
    if refresh.read().next().is_none() {
        return;
    }
    request_manual_refresh(&mut atlas, &mut runtime);
}

fn request_manual_refresh(atlas: &mut AtlasState, runtime: &mut AtlasRuntime) {
    if atlas.connection != BusConnectionState::Connected {
        atlas.notice = Some("Refresh deferred until the local broker reconnects".into());
        atlas.bump();
        return;
    }
    if atlas.refreshing {
        atlas.notice = Some("Mesh refresh already in progress".into());
        atlas.bump();
        return;
    }
    if !runtime.is_idle() {
        atlas.notice = Some("Refresh deferred until the current snapshot read completes".into());
        atlas.bump();
        return;
    }
    begin_refresh(RefreshReason::Manual, atlas, runtime);
}

fn handle_inspector_requests(
    mut requests: MessageReader<InspectCitizen>,
    mut atlas: ResMut<AtlasState>,
    mut runtime: ResMut<AtlasRuntime>,
) {
    for request in requests.read() {
        let Some(identity) = selected_citizen_identity(&atlas, &request.service) else {
            atlas.notice = Some("Citizen is no longer present in the selected snapshot".into());
            atlas.bump();
            continue;
        };
        atlas.select_citizen(identity);
        if !selected_service_is_local(&atlas, &request.service) {
            if let Some(inspector) = atlas.inspector.as_mut() {
                inspector.description_error =
                    Some("Citizen mutation and inspection are same-node only".into());
            }
            atlas.bump();
            continue;
        }
        queue_inspector_snapshot(&request.service, &mut atlas, &mut runtime);
    }
}

fn handle_inspector_mutations(
    mut requests: MessageReader<InspectorMutationRequest>,
    mut atlas: ResMut<AtlasState>,
    mut runtime: ResMut<AtlasRuntime>,
) {
    for request in requests.read() {
        let mutation = &request.0;
        if !mutation_is_current(&atlas, mutation.identity()) {
            set_inspector_result(
                &mut atlas,
                "Mutation rejected: citizen is not the selected same-node process".into(),
                false,
                None,
            );
            continue;
        }
        let target = mutation.target();
        if atlas.active_mutations.contains(&target) {
            set_inspector_result(
                &mut atlas,
                "Mutation rejected: this target already has a queued or in-flight mutation".into(),
                false,
                None,
            );
            continue;
        }
        let operation = runtime.next_operation_id;
        let queued = match mutation {
            InspectorMutation::InvokeAction {
                service,
                action,
                identity,
            } => enqueue_mutation(
                &mut runtime,
                atlas.connection_generation,
                service.clone(),
                "action.invoke",
                RequestKind::ActionInvoke {
                    service: service.clone(),
                    action: action.clone(),
                },
                serde_json::json!({"id": action}).to_string(),
                MutationGuard {
                    identity: identity.clone(),
                    target: target.clone(),
                    operation,
                    invalidated: false,
                },
            ),
            InspectorMutation::SetControl {
                service,
                control,
                value,
                identity,
            } => enqueue_mutation(
                &mut runtime,
                atlas.connection_generation,
                service.clone(),
                "app.controls.set",
                RequestKind::ControlSet {
                    service: service.clone(),
                    control: control.clone(),
                },
                serde_json::json!({"target": control, "value": value}).to_string(),
                MutationGuard {
                    identity: identity.clone(),
                    target: target.clone(),
                    operation,
                    invalidated: false,
                },
            ),
        };
        if queued {
            runtime.next_operation_id = runtime.next_operation_id.wrapping_add(1);
            atlas.active_mutations.insert(target);
            set_inspector_result(
                &mut atlas,
                "Confirmed mutation in progress".into(),
                true,
                None,
            );
        } else {
            set_inspector_result(
                &mut atlas,
                "Mutation rejected: target is already active or the queue is full".into(),
                false,
                None,
            );
        }
    }
}

fn begin_refresh(reason: RefreshReason, atlas: &mut AtlasState, runtime: &mut AtlasRuntime) {
    runtime.refresh_epoch = runtime.refresh_epoch.wrapping_add(1);
    runtime.clear_work();
    atlas.active_mutations.clear();
    atlas.refreshing = true;
    atlas.last_refresh_reason = Some(reason);
    atlas.notice = Some(format!("{} mesh refresh in progress", reason.label()));
    atlas.bump();
    let generation = atlas.connection_generation;
    enqueue_call(
        runtime,
        generation,
        RequestPriority::Bootstrap,
        "noded",
        "noded.inventory",
        RequestKind::Inventory,
        "{}",
    );
    enqueue_call(
        runtime,
        generation,
        RequestPriority::Bootstrap,
        "noded",
        "noded.peers",
        RequestKind::Peers,
        "{}",
    );
}

fn enqueue_call(
    runtime: &mut AtlasRuntime,
    generation: u64,
    priority: RequestPriority,
    to: impl Into<String>,
    command: impl Into<String>,
    kind: RequestKind,
    body: impl Into<String>,
) -> bool {
    if runtime.queued.contains_key(&kind) || runtime.in_flight.contains(&kind) {
        runtime.reread_needed.insert(kind);
        return true;
    }
    if runtime.queued.len() >= MAX_QUEUED {
        return false;
    }
    let sequence = runtime.next_sequence;
    runtime.next_sequence = runtime.next_sequence.wrapping_add(1);
    let call = QueuedCall {
        generation,
        refresh_epoch: runtime.refresh_epoch,
        sequence,
        priority,
        to: to.into(),
        command: command.into(),
        headers: BTreeMap::new(),
        body: body.into(),
        kind: kind.clone(),
        mutation: None,
    };
    runtime.latest_sequence.insert(kind.clone(), sequence);
    runtime.queued.insert(kind.clone(), call);
    match priority {
        RequestPriority::Bootstrap => runtime.bootstrap_queue.push_back(kind),
        RequestPriority::Local => runtime.local_queue.push_back(kind),
        RequestPriority::Remote => runtime.remote_queue.push_back(kind),
    }
    true
}

#[allow(clippy::too_many_arguments)]
fn enqueue_mutation(
    runtime: &mut AtlasRuntime,
    generation: u64,
    to: impl Into<String>,
    command: impl Into<String>,
    kind: RequestKind,
    body: impl Into<String>,
    mutation: MutationGuard,
) -> bool {
    if runtime.queued.contains_key(&kind)
        || runtime.in_flight.contains(&kind)
        || runtime.queued.len() >= MAX_QUEUED
    {
        return false;
    }
    if !enqueue_call(
        runtime,
        generation,
        RequestPriority::Local,
        to,
        command,
        kind.clone(),
        body,
    ) {
        return false;
    }
    runtime
        .queued
        .get_mut(&kind)
        .expect("new mutation remains queued")
        .mutation = Some(mutation);
    true
}

fn pop_queue(
    order: &mut VecDeque<RequestKind>,
    queued: &mut HashMap<RequestKind, QueuedCall>,
) -> Option<QueuedCall> {
    while let Some(kind) = order.pop_front() {
        if let Some(call) = queued.remove(&kind) {
            return Some(call);
        }
    }
    None
}

fn next_call(runtime: &mut AtlasRuntime, allow_remote: bool) -> Option<QueuedCall> {
    if let Some(call) = pop_queue(&mut runtime.bootstrap_queue, &mut runtime.queued) {
        return Some(call);
    }
    if let Some(call) = pop_queue(&mut runtime.local_queue, &mut runtime.queued) {
        return Some(call);
    }
    if runtime.has_local_work() || !allow_remote {
        return None;
    }
    pop_queue(&mut runtime.remote_queue, &mut runtime.queued)
}

fn restore_front(runtime: &mut AtlasRuntime, call: QueuedCall) {
    let kind = call.kind.clone();
    match call.priority {
        RequestPriority::Bootstrap => runtime.bootstrap_queue.push_front(kind.clone()),
        RequestPriority::Local => runtime.local_queue.push_front(kind.clone()),
        RequestPriority::Remote => runtime.remote_queue.push_front(kind.clone()),
    }
    runtime.queued.insert(kind, call);
}

fn pump_requests(
    bridge: Option<Res<BusBridge>>,
    mut atlas: ResMut<AtlasState>,
    mut runtime: ResMut<AtlasRuntime>,
) {
    let Some(bridge) = bridge else {
        return;
    };
    if atlas.connection != BusConnectionState::Connected {
        return;
    }
    while runtime.pending.len() < MAX_INFLIGHT {
        if !runtime.has_local_work() && runtime.remote_queue.len() < MAX_INFLIGHT * 2 {
            fill_remote_queue(&atlas, &mut runtime);
        }
        let allow_remote = runtime
            .pending
            .values()
            .filter(|call| call.priority == RequestPriority::Remote)
            .count()
            < REMOTE_MAX_INFLIGHT;
        let Some(call) = next_call(&mut runtime, allow_remote) else {
            break;
        };
        if call
            .mutation
            .as_ref()
            .is_some_and(|guard| !mutation_is_current(&atlas, &guard.identity))
        {
            reject_stale_mutation(&call, &mut atlas, &mut runtime);
            continue;
        }
        let request_id = runtime.next_request_id;
        match bridge.try_call(
            request_id,
            call.to.clone(),
            call.command.clone(),
            call.headers.clone(),
            call.body.clone(),
        ) {
            Ok(()) => {
                runtime.next_request_id = runtime.next_request_id.wrapping_add(1);
                runtime.in_flight.insert(call.kind.clone());
                runtime.pending.insert(request_id, call);
            }
            Err(error) => {
                restore_front(&mut runtime, call);
                atlas.notice = Some(format!("Bus request queue busy: {error}"));
                atlas.bump();
                break;
            }
        }
    }
    finish_refresh_if_idle(&mut atlas, &mut runtime);
}

fn handle_reply(
    request_id: u64,
    result: Result<BusReply, String>,
    atlas: &mut AtlasState,
    runtime: &mut AtlasRuntime,
) {
    let Some(call) = runtime.pending.remove(&request_id) else {
        return;
    };
    runtime.in_flight.remove(&call.kind);
    let reread = runtime.reread_needed.remove(&call.kind);
    if let Some(guard) = &call.mutation {
        atlas.active_mutations.remove(&guard.target);
        atlas.bump();
    }
    if !accepts_response(
        atlas.connection_generation,
        runtime.refresh_epoch,
        &runtime.latest_sequence,
        &call,
    ) {
        return;
    }
    if let Some(guard) = &call.mutation {
        if guard.invalidated || !mutation_is_current(atlas, &guard.identity) {
            atlas.notice = Some(format!(
                "Mutation response for {} discarded: selected process identity changed after dispatch",
                guard.identity.service
            ));
            atlas.bump();
            return;
        }
    }
    let reply = match result {
        Ok(reply) if reply.rc == 0 => Some(reply),
        Ok(reply) => {
            let detail = reply.result.unwrap_or(reply.body);
            record_failure(
                &call.kind,
                call.mutation.as_ref().map(|guard| guard.operation),
                format!("RC {}: {detail}", reply.rc),
                atlas,
                runtime,
            );
            None
        }
        Err(error) => {
            record_failure(
                &call.kind,
                call.mutation.as_ref().map(|guard| guard.operation),
                error,
                atlas,
                runtime,
            );
            None
        }
    };
    if let Some(reply) = reply {
        apply_reply_guarded(
            &call.kind,
            call.mutation.as_ref().map(|guard| guard.operation),
            &reply.body,
            atlas,
            runtime,
        );
    }
    if reread {
        enqueue_call(
            runtime,
            call.generation,
            call.priority,
            call.to,
            call.command,
            call.kind,
            call.body,
        );
    }
}

fn handle_observation_connection(
    state: BusConnectionState,
    generation: u64,
    traffic: &mut TrafficState,
    runtime: &mut TrafficRuntime,
) {
    traffic.observation_connection = state;
    match state {
        BusConnectionState::Connected if traffic.connection_generation != generation => {
            runtime.pending.clear();
            runtime.desired_stop = None;
            runtime.desired_start = traffic.connected(generation);
        }
        BusConnectionState::Disconnected
        | BusConnectionState::ShuttingDown
        | BusConnectionState::Fatal => {
            runtime.pending.clear();
            runtime.desired_stop = None;
            runtime.desired_start = false;
            traffic.disconnected();
        }
        BusConnectionState::Connecting | BusConnectionState::Connected => {}
    }
}

fn handle_observation_reply(
    request_id: u64,
    result: Result<BusReply, String>,
    traffic: &mut TrafficState,
    runtime: &mut TrafficRuntime,
) {
    let Some(pending) = runtime.pending.remove(&request_id) else {
        return;
    };
    if pending.generation != traffic.connection_generation {
        return;
    }
    let failure = |reply: BusReply| {
        let detail = reply.result.unwrap_or(reply.body);
        format!("RC {}: {detail}", reply.rc)
    };
    match pending.call {
        TrafficCall::Start { revision } => match result {
            Ok(reply) if reply.rc == 0 => {
                if let Err(error) = traffic.start_succeeded(&reply.body) {
                    traffic.request_failed(error);
                    return;
                }
                if !traffic.open || revision != traffic.filter_revision {
                    if let Some(subscription_id) = traffic.subscription_id.clone() {
                        runtime.desired_stop = Some(subscription_id);
                        traffic.stop_pending(false);
                    }
                }
            }
            Ok(reply) => traffic.request_failed(failure(reply)),
            Err(error) => traffic.request_failed(error),
        },
        TrafficCall::Stop { subscription_id } => match result {
            Ok(reply) if reply.rc == 0 => {
                traffic.stop_succeeded();
                if traffic.open {
                    runtime.desired_start = true;
                }
            }
            Ok(reply) => {
                traffic.subscription_id = Some(subscription_id);
                traffic.stop_failed(failure(reply));
            }
            Err(error) => {
                traffic.subscription_id = Some(subscription_id);
                traffic.stop_failed(error);
            }
        },
    }
}

#[cfg(test)]
fn apply_reply(kind: &RequestKind, body: &str, atlas: &mut AtlasState, runtime: &mut AtlasRuntime) {
    apply_reply_guarded(kind, None, body, atlas, runtime);
}

fn apply_reply_guarded(
    kind: &RequestKind,
    operation: Option<u64>,
    body: &str,
    atlas: &mut AtlasState,
    runtime: &mut AtlasRuntime,
) {
    let observed_at_ms = now_unix_ms();
    match kind {
        RequestKind::Inventory => match InventoryProjection::parse(body) {
            Ok(inventory) => {
                runtime.inventory_verified = atlas.apply_inventory(inventory, observed_at_ms);
                if !runtime.inventory_verified {
                    let reason = atlas
                        .mesh_reason
                        .clone()
                        .unwrap_or_else(|| "inventory is unverified".into());
                    mark_refresh_failed(runtime, reason);
                }
                accept_verified_peers_and_queue_fanout(atlas, runtime);
            }
            Err(error) => record_failure(kind, operation, error, atlas, runtime),
        },
        RequestKind::Peers => match PeersProjection::parse(body) {
            Ok(peers) => {
                runtime.peers_ready = true;
                runtime.staged_peers = Some(peers);
                accept_verified_peers_and_queue_fanout(atlas, runtime);
            }
            Err(error) => record_failure(kind, operation, error, atlas, runtime),
        },
        RequestKind::NodeInfo { node } => match serde_json::from_str::<NodeInfo>(body) {
            Ok(info) => atlas.observe_node_info(node, info, observed_at_ms),
            Err(error) => {
                atlas.mark_node_info_unknown(node, format!("invalid noded.info: {error}"))
            }
        },
        RequestKind::NodeList { node } => match parse_service_list(body) {
            Ok(citizens) => {
                let is_local = atlas.local_node() == Some(node.as_str());
                atlas.observe_citizens(node, citizens.clone(), observed_at_ms);
                if is_local {
                    queue_local_props(&citizens, atlas, runtime);
                    if let Some(service) = atlas.selected_citizen.clone() {
                        if citizens.iter().any(|citizen| citizen.name == service) {
                            queue_inspector_snapshot(&service, atlas, runtime);
                        }
                    }
                }
            }
            Err(error) => atlas.mark_node_citizens_unknown(node, error),
        },
        RequestKind::PropsList { service } => match parse_path_list(body) {
            Ok(paths) => {
                let Some(surface) = atlas.properties.get_mut(service) else {
                    return;
                };
                surface.availability = PropsAvailability::Available;
                surface.paths = paths;
                surface
                    .descriptions
                    .retain(|path, _| surface.paths.binary_search(path).is_ok());
                surface.observed_at_ms = Some(observed_at_ms);
                let paths = surface.paths.clone();
                atlas.bump();
                for path in paths {
                    enqueue_call(
                        runtime,
                        atlas.connection_generation,
                        RequestPriority::Local,
                        service.clone(),
                        format!("{service}.props.describe"),
                        RequestKind::PropsDescribe {
                            service: service.clone(),
                            path: path.clone(),
                        },
                        serde_json::json!({"path": path}).to_string(),
                    );
                }
            }
            Err(error) => mark_props_unavailable(atlas, service, error),
        },
        RequestKind::PropsGet { service } => match serde_json::from_str::<Value>(body) {
            Ok(snapshot) => {
                let Some(surface) = atlas.properties.get_mut(service) else {
                    return;
                };
                surface.availability = PropsAvailability::Available;
                surface.snapshot = Some(snapshot);
                surface.observed_at_ms = Some(observed_at_ms);
                atlas.bump();
            }
            Err(error) => mark_props_unavailable(
                atlas,
                service,
                format!("invalid props.get response: {error}"),
            ),
        },
        RequestKind::PropsDescribe { service, path } => match serde_json::from_str::<Value>(body) {
            Ok(description) => {
                let Some(surface) = atlas.properties.get_mut(service) else {
                    return;
                };
                if surface.paths.binary_search(path).is_ok() {
                    surface.descriptions.insert(path.clone(), description);
                    surface.observed_at_ms = Some(observed_at_ms);
                    atlas.bump();
                }
            }
            Err(error) => mark_props_unavailable(
                atlas,
                service,
                format!("invalid props.describe response: {error}"),
            ),
        },
        RequestKind::AppDescribe { service } => match AppDescription::parse(body) {
            Ok(description) => {
                let has_controls = description.has_verb("app.controls.list");
                if let Some(inspector) = selected_inspector_mut(atlas, service) {
                    inspector.description = Some(description);
                    inspector.description_error = None;
                    inspector.description_observed_at_ms = Some(observed_at_ms);
                    if !has_controls {
                        inspector.controls.clear();
                        inspector.controls_error =
                            Some("Citizen does not advertise CTK controls".into());
                        inspector.controls_observed_at_ms = Some(observed_at_ms);
                    }
                    atlas.bump();
                }
                if has_controls {
                    enqueue_call(
                        runtime,
                        atlas.connection_generation,
                        RequestPriority::Local,
                        service.clone(),
                        "app.controls.list",
                        RequestKind::ControlsList {
                            service: service.clone(),
                        },
                        "{}",
                    );
                }
            }
            Err(error) => mark_inspector_failure(kind, operation, error, atlas, runtime),
        },
        RequestKind::ActionsList { service } => match parse_actions_list(body) {
            Ok(parsed) => {
                let action_ids: Vec<_> = parsed
                    .actions
                    .iter()
                    .map(|action| action.id.clone())
                    .collect();
                if let Some(inspector) = selected_inspector_mut(atlas, service) {
                    inspector.actions = parsed
                        .actions
                        .into_iter()
                        .map(|action| (action.id.clone(), action))
                        .collect();
                    inspector.actions_omitted = parsed.omitted;
                    inspector.actions_error = None;
                    inspector.actions_observed_at_ms = Some(observed_at_ms);
                    atlas.bump();
                }
                for action in action_ids {
                    enqueue_call(
                        runtime,
                        atlas.connection_generation,
                        RequestPriority::Local,
                        service.clone(),
                        "actions.describe",
                        RequestKind::ActionDescribe {
                            service: service.clone(),
                            action: action.clone(),
                        },
                        serde_json::json!({"id": action}).to_string(),
                    );
                }
            }
            Err(error) => mark_inspector_failure(kind, operation, error, atlas, runtime),
        },
        RequestKind::ActionDescribe { service, action } => match parse_action_description(body) {
            Ok(description) => {
                if description.id == *action {
                    if let Some(inspector) = selected_inspector_mut(atlas, service) {
                        if inspector.actions.contains_key(action) {
                            inspector.actions.insert(action.clone(), description);
                            inspector.actions_observed_at_ms = Some(observed_at_ms);
                            atlas.bump();
                        }
                    }
                }
            }
            Err(error) => mark_inspector_failure(kind, operation, error, atlas, runtime),
        },
        RequestKind::ControlsList { service } => match parse_controls_list(body) {
            Ok(parsed) => {
                let queryable: Vec<_> = parsed
                    .controls
                    .iter()
                    .filter(|control| control.queryable)
                    .map(|control| control.id.clone())
                    .collect();
                if let Some(inspector) = selected_inspector_mut(atlas, service) {
                    inspector.controls = parsed
                        .controls
                        .into_iter()
                        .map(|control| (control.id.clone(), control))
                        .collect();
                    inspector.controls_omitted = parsed.omitted;
                    inspector.controls_error = None;
                    inspector.controls_observed_at_ms = Some(observed_at_ms);
                    atlas.bump();
                }
                for control in queryable {
                    queue_control_get(runtime, atlas.connection_generation, service, &control);
                }
            }
            Err(error) => mark_inspector_failure(kind, operation, error, atlas, runtime),
        },
        RequestKind::ControlGet { service, control } => match parse_control_value(body) {
            Ok((returned, value)) if returned == *control => {
                if let Some(inspector) = selected_inspector_mut(atlas, service) {
                    if let Some(control) = inspector.controls.get_mut(control) {
                        control.value = Some(value);
                        control.value_error = None;
                        control.value_observed_at_ms = Some(observed_at_ms);
                        atlas.bump();
                    }
                }
            }
            Ok((returned, _)) => mark_inspector_failure(
                kind,
                operation,
                format!("app.controls.get returned {returned}, expected {control}"),
                atlas,
                runtime,
            ),
            Err(error) => mark_inspector_failure(kind, operation, error, atlas, runtime),
        },
        RequestKind::ActionInvoke { service, action } => {
            let body = parse_inspector_result_body(body);
            if operation.is_some_and(|operation| operation_is_current(runtime, operation)) {
                set_inspector_result_for(
                    atlas,
                    service,
                    format!("action.invoke {action} accepted"),
                    true,
                    body,
                    observed_at_ms,
                );
            }
        }
        RequestKind::ControlSet { service, control } => {
            let body = parse_inspector_result_body(body);
            if operation.is_some_and(|operation| operation_is_current(runtime, operation)) {
                set_inspector_result_for(
                    atlas,
                    service,
                    format!("app.controls.set {control} accepted"),
                    true,
                    body,
                    observed_at_ms,
                );
            }
            queue_control_get(runtime, atlas.connection_generation, service, control);
        }
    }
}

fn accept_verified_peers_and_queue_fanout(atlas: &mut AtlasState, runtime: &mut AtlasRuntime) {
    if runtime.fanout_scheduled || !runtime.inventory_verified || !runtime.peers_ready {
        return;
    }
    let Some(peers) = runtime.staged_peers.take() else {
        return;
    };
    atlas.observe_peers(peers);
    atlas.bump();
    let Some(local) = atlas.local_node().map(str::to_owned) else {
        return;
    };
    runtime.fanout_scheduled = true;
    queue_node_snapshot(
        runtime,
        atlas.connection_generation,
        &local,
        RequestPriority::Local,
    );
    runtime.remote_fanout_active = true;
    runtime.remote_cursor = 0;
    runtime.remote_local = Some(local);
}

fn fill_remote_queue(atlas: &AtlasState, runtime: &mut AtlasRuntime) {
    let Some(inventory) = atlas.inventory.as_ref() else {
        runtime.remote_fanout_active = false;
        return;
    };
    while runtime.remote_queue.len() < MAX_INFLIGHT * 2
        && runtime.queued.len() + 2 <= MAX_QUEUED
        && runtime.remote_cursor < inventory.members.len()
    {
        let member = &inventory.members[runtime.remote_cursor];
        runtime.remote_cursor += 1;
        if !member.active_bus() || runtime.remote_local.as_deref() == Some(member.name.as_str()) {
            continue;
        }
        queue_node_snapshot(
            runtime,
            atlas.connection_generation,
            &member.name,
            RequestPriority::Remote,
        );
    }
    if runtime.remote_cursor >= inventory.members.len() {
        runtime.remote_fanout_active = false;
    }
}

fn queue_node_snapshot(
    runtime: &mut AtlasRuntime,
    generation: u64,
    node: &str,
    priority: RequestPriority,
) {
    let target = format!("{node}.bus");
    enqueue_call(
        runtime,
        generation,
        priority,
        target.clone(),
        "noded.info",
        RequestKind::NodeInfo {
            node: node.to_owned(),
        },
        "{}",
    );
    enqueue_call(
        runtime,
        generation,
        priority,
        target,
        "noded.list",
        RequestKind::NodeList {
            node: node.to_owned(),
        },
        "{}",
    );
}

fn queue_local_props(
    citizens: &[crate::model::ServiceInfo],
    atlas: &mut AtlasState,
    runtime: &mut AtlasRuntime,
) {
    let mut services: Vec<String> = citizens
        .iter()
        .map(|service| service.name.clone())
        .filter(|service| is_flat_surface(service) || is_namespaced_surface(service))
        .collect();
    if !services.iter().any(|service| service == "noded") {
        services.push("noded".into());
    }
    services.sort();
    services.dedup();
    let retained: HashSet<_> = services.iter().cloned().collect();
    atlas
        .properties
        .retain(|service, _| retained.contains(service));
    atlas.bump();
    for service in services {
        if is_namespaced_surface(&service) {
            atlas
                .properties
                .insert(service, PropsSurface::namespace_required());
            atlas.bump();
            continue;
        }
        atlas
            .properties
            .entry(service.clone())
            .or_insert_with(PropsSurface::pending);
        enqueue_call(
            runtime,
            atlas.connection_generation,
            RequestPriority::Local,
            service.clone(),
            format!("{service}.props.list"),
            RequestKind::PropsList {
                service: service.clone(),
            },
            "{}",
        );
        enqueue_call(
            runtime,
            atlas.connection_generation,
            RequestPriority::Local,
            service.clone(),
            format!("{service}.props.get"),
            RequestKind::PropsGet { service },
            "{}",
        );
    }
}

fn selected_service_is_local(atlas: &AtlasState, service: &str) -> bool {
    let Some(local) = atlas.local_node() else {
        return false;
    };
    atlas.selected.as_deref() == Some(local)
        && atlas
            .nodes
            .get(local)
            .is_some_and(|node| node.citizens.iter().any(|citizen| citizen.name == service))
}

fn selected_citizen_identity(atlas: &AtlasState, service: &str) -> Option<ProcessIdentity> {
    let selected = atlas.selected.as_deref()?;
    atlas
        .nodes
        .get(selected)?
        .citizens
        .iter()
        .find(|citizen| citizen.name == service)
        .map(crate::model::ServiceInfo::process_identity)
}

fn mutation_is_current(atlas: &AtlasState, identity: &ProcessIdentity) -> bool {
    selected_service_is_local(atlas, &identity.service)
        && atlas.selected_citizen.as_deref() == Some(identity.service.as_str())
        && atlas
            .selected_process_identity(&identity.service)
            .is_some_and(|current| identity.same_process(&current))
}

fn purge_invalid_queued_mutations(
    mut atlas: ResMut<AtlasState>,
    mut runtime: ResMut<AtlasRuntime>,
) {
    purge_invalid_queued_mutations_inner(&mut atlas, &mut runtime);
}

fn purge_invalid_queued_mutations_inner(atlas: &mut AtlasState, runtime: &mut AtlasRuntime) {
    let stale: Vec<_> = runtime
        .queued
        .iter()
        .filter_map(|(kind, call)| {
            call.mutation
                .as_ref()
                .filter(|guard| !mutation_is_current(atlas, &guard.identity))
                .map(|_| kind.clone())
        })
        .collect();
    for kind in stale {
        if let Some(call) = runtime.queued.remove(&kind) {
            reject_stale_mutation(&call, atlas, runtime);
        }
    }

    let mut invalidated = Vec::new();
    for call in runtime.pending.values_mut() {
        let Some(guard) = call.mutation.as_mut() else {
            continue;
        };
        if !guard.invalidated && !mutation_is_current(atlas, &guard.identity) {
            guard.invalidated = true;
            invalidated.push(guard.identity.service.clone());
        }
    }
    for service in invalidated {
        atlas.notice = Some(format!(
            "In-flight mutation response for {service} will be discarded: selected process identity is stale"
        ));
        atlas.bump();
    }
}

fn reject_stale_mutation(call: &QueuedCall, atlas: &mut AtlasState, runtime: &mut AtlasRuntime) {
    let Some(guard) = &call.mutation else {
        return;
    };
    runtime.latest_sequence.remove(&call.kind);
    runtime.reread_needed.remove(&call.kind);
    atlas.active_mutations.remove(&guard.target);
    if atlas
        .inspector
        .as_ref()
        .is_some_and(|inspector| inspector.identity == guard.identity)
    {
        set_inspector_result_for(
            atlas,
            &guard.identity.service,
            "Mutation not executed: selected process identity is stale".into(),
            false,
            None,
            now_unix_ms(),
        );
    }
    atlas.notice = Some(format!(
        "Mutation for {} not executed: selected process identity is stale",
        guard.identity.service
    ));
    atlas.bump();
}

fn selected_inspector_mut<'a>(
    atlas: &'a mut AtlasState,
    service: &str,
) -> Option<&'a mut CitizenInspector> {
    atlas
        .inspector
        .as_mut()
        .filter(|inspector| inspector.service == service)
}

fn queue_inspector_snapshot(service: &str, atlas: &mut AtlasState, runtime: &mut AtlasRuntime) {
    if let Some(inspector) = selected_inspector_mut(atlas, service) {
        inspector.description_error = None;
        inspector.actions_error = None;
        inspector.controls_error = None;
        atlas.bump();
    }
    for (command, kind) in [
        (
            "app.describe",
            RequestKind::AppDescribe {
                service: service.to_owned(),
            },
        ),
        (
            "actions.list",
            RequestKind::ActionsList {
                service: service.to_owned(),
            },
        ),
    ] {
        if !enqueue_call(
            runtime,
            atlas.connection_generation,
            RequestPriority::Local,
            service,
            command,
            kind,
            "{}",
        ) {
            if let Some(inspector) = selected_inspector_mut(atlas, service) {
                inspector.description_error = Some("Inspector request queue is full".into());
                atlas.bump();
            }
        }
    }
}

fn queue_control_get(runtime: &mut AtlasRuntime, generation: u64, service: &str, control: &str) {
    enqueue_call(
        runtime,
        generation,
        RequestPriority::Local,
        service,
        "app.controls.get",
        RequestKind::ControlGet {
            service: service.to_owned(),
            control: control.to_owned(),
        },
        serde_json::json!({"target": control}).to_string(),
    );
}

fn set_inspector_result(atlas: &mut AtlasState, summary: String, ok: bool, body: Option<Value>) {
    let Some(service) = atlas.selected_citizen.clone() else {
        return;
    };
    set_inspector_result_for(atlas, &service, summary, ok, body, now_unix_ms());
}

fn set_inspector_result_for(
    atlas: &mut AtlasState,
    service: &str,
    mut summary: String,
    ok: bool,
    body: Option<Value>,
    observed_at_ms: u64,
) {
    truncate_text(&mut summary, 512);
    if let Some(inspector) = selected_inspector_mut(atlas, service) {
        inspector.result = Some(InspectorResult {
            summary,
            ok,
            body,
            observed_at_ms,
        });
        atlas.bump();
    }
}

fn parse_inspector_result_body(body: &str) -> Option<Value> {
    const MAX_RESULT_BYTES: usize = 4 * 1024;
    if body.len() > MAX_RESULT_BYTES {
        return Some(serde_json::json!({"omitted": "oversize"}));
    }
    serde_json::from_str::<Value>(body).ok()
}

fn operation_is_current(runtime: &AtlasRuntime, operation: u64) -> bool {
    runtime.next_operation_id.wrapping_sub(1) == operation
}

fn mark_inspector_failure(
    kind: &RequestKind,
    operation: Option<u64>,
    mut error: String,
    atlas: &mut AtlasState,
    runtime: &mut AtlasRuntime,
) {
    truncate_text(&mut error, 512);
    let observed_at_ms = now_unix_ms();
    match kind {
        RequestKind::AppDescribe { service } => {
            if let Some(inspector) = selected_inspector_mut(atlas, service) {
                inspector.description_error = Some(error);
                inspector.description_observed_at_ms = Some(observed_at_ms);
                atlas.bump();
            }
        }
        RequestKind::ActionsList { service } | RequestKind::ActionDescribe { service, .. } => {
            if let Some(inspector) = selected_inspector_mut(atlas, service) {
                inspector.actions_error = Some(error);
                inspector.actions_observed_at_ms = Some(observed_at_ms);
                atlas.bump();
            }
        }
        RequestKind::ControlsList { service } => {
            if let Some(inspector) = selected_inspector_mut(atlas, service) {
                inspector.controls_error = Some(error);
                inspector.controls_observed_at_ms = Some(observed_at_ms);
                atlas.bump();
            }
        }
        RequestKind::ControlGet { service, control } => {
            if let Some(inspector) = selected_inspector_mut(atlas, service) {
                if let Some(control) = inspector.controls.get_mut(control) {
                    control.value_error = Some(error);
                    control.value_observed_at_ms = Some(observed_at_ms);
                    atlas.bump();
                }
            }
        }
        RequestKind::ActionInvoke { service, action }
            if operation.is_some_and(|operation| operation_is_current(runtime, operation)) =>
        {
            set_inspector_result_for(
                atlas,
                service,
                format!("action.invoke {action} failed: {error}"),
                false,
                None,
                observed_at_ms,
            )
        }
        RequestKind::ControlSet { service, control } => {
            if operation.is_some_and(|operation| operation_is_current(runtime, operation)) {
                set_inspector_result_for(
                    atlas,
                    service,
                    format!("app.controls.set {control} failed: {error}"),
                    false,
                    None,
                    observed_at_ms,
                );
            }
            queue_control_get(runtime, atlas.connection_generation, service, control);
        }
        _ => {}
    }
}

fn handle_topic(message: BusMessage, atlas: &mut AtlasState, runtime: &mut AtlasRuntime) {
    if message.connection_generation != atlas.connection_generation {
        return;
    }
    let Some(topic) = message.topic() else {
        return;
    };
    let service = retained_service(topic).or_else(|| {
        if topic == INTERACT_CHANGED {
            Some("interact")
        } else {
            None
        }
    });
    let Some(service) = service else {
        return;
    };
    if !enqueue_call(
        runtime,
        atlas.connection_generation,
        RequestPriority::Local,
        service,
        format!("{service}.props.get"),
        RequestKind::PropsGet {
            service: service.into(),
        },
        "{}",
    ) {
        atlas.notice = Some(format!("snapshot queue full; {service} refresh deferred"));
        atlas.bump();
    }
}

fn accepts_response(
    generation: u64,
    refresh_epoch: u64,
    latest_sequence: &HashMap<RequestKind, u64>,
    call: &QueuedCall,
) -> bool {
    call.generation == generation
        && call.refresh_epoch == refresh_epoch
        && latest_sequence.get(&call.kind) == Some(&call.sequence)
}

fn record_failure(
    kind: &RequestKind,
    operation: Option<u64>,
    error: String,
    atlas: &mut AtlasState,
    runtime: &mut AtlasRuntime,
) {
    match kind {
        RequestKind::NodeInfo { node } => atlas.mark_node_info_unknown(node, error),
        RequestKind::NodeList { node } => atlas.mark_node_citizens_unknown(node, error),
        RequestKind::PropsList { service }
        | RequestKind::PropsGet { service }
        | RequestKind::PropsDescribe { service, .. } => {
            mark_props_unavailable(atlas, service, error);
        }
        RequestKind::Inventory => {
            atlas.mark_inventory_failed(error.clone());
            mark_refresh_failed(runtime, error);
        }
        RequestKind::Peers => {
            atlas.mark_all_remote_stale();
            mark_refresh_failed(runtime, error);
        }
        RequestKind::AppDescribe { .. }
        | RequestKind::ActionsList { .. }
        | RequestKind::ActionDescribe { .. }
        | RequestKind::ControlsList { .. }
        | RequestKind::ControlGet { .. }
        | RequestKind::ActionInvoke { .. }
        | RequestKind::ControlSet { .. } => {
            mark_inspector_failure(kind, operation, error, atlas, runtime);
        }
    }
}

fn mark_refresh_failed(runtime: &mut AtlasRuntime, error: String) {
    if runtime.refresh_failure.is_none() {
        runtime.refresh_failure = Some(error);
    }
}

fn mark_props_unavailable(atlas: &mut AtlasState, service: &str, error: String) {
    if let Some(surface) = atlas.properties.get_mut(service) {
        surface.availability = PropsAvailability::Unavailable(error);
        atlas.bump();
    }
}

fn finish_refresh_if_idle(atlas: &mut AtlasState, runtime: &mut AtlasRuntime) {
    if atlas.refreshing && runtime.is_idle() {
        atlas.refreshing = false;
        atlas.notice = Some(if let Some(error) = runtime.refresh_failure.take() {
            format!("Mesh refresh failed: {error}")
        } else {
            "Mesh refresh complete".into()
        });
        atlas.bump();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(name: &str, pid: u32) -> crate::model::ServiceInfo {
        crate::model::ServiceInfo {
            name: name.into(),
            binary: Some("cosmix-studio".into()),
            version: Some("0.2.0".into()),
            git_sha: None,
            git_dirty: None,
            pid: Some(pid),
            started_at: Some(format!("process-{pid}")),
        }
    }

    fn selected_local_citizen() -> (AtlasState, ProcessIdentity) {
        let mut atlas = AtlasState::default();
        assert!(atlas.apply_inventory(
            InventoryProjection::parse(include_str!("fixtures/inventory_verified.json")).unwrap(),
            500,
        ));
        atlas.peers = Some(PeersProjection::parse(r#"{"node":"alpha","peers":[]}"#).unwrap());
        atlas.select("alpha");
        let citizen = service("studio-bevy-4242", 4242);
        let identity = citizen.process_identity();
        atlas.observe_citizens("alpha", vec![citizen], 600);
        atlas.select_citizen(identity.clone());
        (atlas, identity)
    }

    #[test]
    fn real_ctk_provenance_is_current_but_name_only_identity_is_not() {
        let mut atlas = AtlasState::default();
        assert!(atlas.apply_inventory(
            InventoryProjection::parse(include_str!("fixtures/inventory_verified.json")).unwrap(),
            500,
        ));
        atlas.peers = Some(PeersProjection::parse(r#"{"node":"alpha","peers":[]}"#).unwrap());
        atlas.select("alpha");

        let citizens =
            crate::model::parse_service_list(include_str!("fixtures/noded_list_ctk.json")).unwrap();
        let identity = citizens[0].process_identity();
        atlas.observe_citizens("alpha", citizens, 600);
        atlas.select_citizen(identity.clone());
        assert!(
            mutation_is_current(&atlas, &identity),
            "a real noded.list CTK process identity must be mutable"
        );

        let unknown = crate::model::parse_service_list(r#"["tower-bevy-4242"]"#).unwrap();
        let unknown_identity = unknown[0].process_identity();
        atlas.observe_citizens("alpha", unknown, 700);
        atlas.select_citizen(unknown_identity.clone());
        assert!(
            !mutation_is_current(&atlas, &unknown_identity),
            "a provenance-less citizen must remain fail-closed"
        );
    }

    fn call(generation: u64, refresh_epoch: u64, sequence: u64, kind: RequestKind) -> QueuedCall {
        QueuedCall {
            generation,
            refresh_epoch,
            sequence,
            priority: RequestPriority::Bootstrap,
            to: "noded".into(),
            command: "noded.inventory".into(),
            headers: BTreeMap::new(),
            body: "{}".into(),
            kind,
            mutation: None,
        }
    }

    #[test]
    fn rejects_old_connections_epochs_and_same_key_sequences() {
        let kind = RequestKind::Inventory;
        let latest = HashMap::from([(kind.clone(), 9)]);
        assert!(accepts_response(
            4,
            8,
            &latest,
            &call(4, 8, 9, kind.clone())
        ));
        assert!(!accepts_response(
            4,
            8,
            &latest,
            &call(3, 8, 9, kind.clone())
        ));
        assert!(!accepts_response(
            4,
            8,
            &latest,
            &call(4, 7, 9, kind.clone())
        ));
        assert!(!accepts_response(4, 8, &latest, &call(4, 8, 8, kind)));
    }

    #[test]
    fn queue_is_bounded_and_duplicate_keys_coalesce_to_one_reread() {
        let mut runtime = AtlasRuntime {
            refresh_epoch: 1,
            ..Default::default()
        };
        for index in 0..(MAX_QUEUED + 20) {
            enqueue_call(
                &mut runtime,
                1,
                RequestPriority::Remote,
                "node.bus",
                "noded.info",
                RequestKind::NodeInfo {
                    node: format!("node-{index}"),
                },
                "{}",
            );
        }
        assert_eq!(runtime.queued.len(), MAX_QUEUED);
        let duplicate = RequestKind::NodeInfo {
            node: "node-0".into(),
        };
        enqueue_call(
            &mut runtime,
            1,
            RequestPriority::Remote,
            "node.bus",
            "noded.info",
            duplicate.clone(),
            "{}",
        );
        assert_eq!(runtime.queued.len(), MAX_QUEUED);
        assert!(runtime.reread_needed.contains(&duplicate));
        assert_eq!(MAX_INFLIGHT, 8);
    }

    #[test]
    fn local_work_precedes_and_blocks_remote_work() {
        let mut runtime = AtlasRuntime::default();
        enqueue_call(
            &mut runtime,
            1,
            RequestPriority::Remote,
            "remote.bus",
            "noded.info",
            RequestKind::NodeInfo {
                node: "remote".into(),
            },
            "{}",
        );
        enqueue_call(
            &mut runtime,
            1,
            RequestPriority::Local,
            "local.bus",
            "noded.list",
            RequestKind::NodeList {
                node: "local".into(),
            },
            "{}",
        );
        let local = next_call(&mut runtime, true).unwrap();
        assert_eq!(local.priority, RequestPriority::Local);
        runtime.in_flight.insert(local.kind.clone());
        runtime.pending.insert(1, local);
        assert!(next_call(&mut runtime, true).is_none());
    }

    #[test]
    fn repeated_manual_refresh_is_rejected_without_replacing_inflight_work() {
        let mut atlas = AtlasState {
            connection: BusConnectionState::Connected,
            connection_generation: 3,
            ..AtlasState::default()
        };
        let mut runtime = AtlasRuntime::default();
        request_manual_refresh(&mut atlas, &mut runtime);
        let epoch = runtime.refresh_epoch;
        let queued = runtime.queued.len();
        request_manual_refresh(&mut atlas, &mut runtime);
        assert_eq!(runtime.refresh_epoch, epoch);
        assert_eq!(runtime.queued.len(), queued);
        assert_eq!(queued, 2);
        assert!(atlas.refreshing);
    }

    #[test]
    fn manual_refresh_does_not_orphan_non_refresh_inflight_calls() {
        let mut atlas = AtlasState {
            connection: BusConnectionState::Connected,
            connection_generation: 3,
            ..AtlasState::default()
        };
        let mut runtime = AtlasRuntime::default();
        let pending = call(
            3,
            0,
            1,
            RequestKind::PropsGet {
                service: "noded".into(),
            },
        );
        runtime.in_flight.insert(pending.kind.clone());
        runtime.pending.insert(1, pending);
        request_manual_refresh(&mut atlas, &mut runtime);
        assert_eq!(runtime.refresh_epoch, 0);
        assert_eq!(runtime.pending.len(), 1);
        assert!(!atlas.refreshing);
    }

    #[test]
    fn unverified_inventory_never_schedules_member_fanout() {
        let mut atlas = AtlasState::default();
        let mut runtime = AtlasRuntime {
            refresh_epoch: 1,
            peers_ready: true,
            staged_peers: Some(
                PeersProjection::parse(r#"{"node":"replacement","peers":[]}"#).unwrap(),
            ),
            ..AtlasRuntime::default()
        };
        atlas.peers = Some(PeersProjection::parse(r#"{"node":"alpha","peers":[]}"#).unwrap());
        apply_reply(
            &RequestKind::Inventory,
            include_str!("fixtures/inventory_unverified.json"),
            &mut atlas,
            &mut runtime,
        );
        assert!(!runtime.inventory_verified);
        assert!(!runtime.fanout_scheduled);
        assert!(runtime.queued.is_empty());
        assert_eq!(atlas.local_node(), Some("alpha"));
    }

    #[test]
    fn topic_event_queues_exactly_one_targeted_snapshot() {
        let mut atlas = AtlasState {
            connection_generation: 7,
            ..AtlasState::default()
        };
        let mut runtime = AtlasRuntime::default();
        handle_topic(
            BusMessage {
                connection_generation: 7,
                from: "noded".into(),
                command: "topic.message".into(),
                body: r#"{"ignored":"retained event body"}"#.into(),
                headers: BTreeMap::from([("topic".into(), "world.noded".into())]),
            },
            &mut atlas,
            &mut runtime,
        );
        assert_eq!(runtime.queued.len(), 1);
        assert!(runtime.queued.contains_key(&RequestKind::PropsGet {
            service: "noded".into()
        }));
    }

    #[test]
    fn fresh_local_roster_and_path_list_prune_absent_state() {
        let mut atlas = AtlasState::default();
        atlas
            .properties
            .insert("indexd".into(), PropsSurface::pending());
        atlas
            .properties
            .insert("musicd".into(), PropsSurface::pending());
        let mut runtime = AtlasRuntime::default();
        queue_local_props(
            &[crate::model::ServiceInfo {
                name: "indexd".into(),
                binary: None,
                version: None,
                git_sha: None,
                git_dirty: None,
                pid: None,
                started_at: None,
            }],
            &mut atlas,
            &mut runtime,
        );
        assert!(atlas.properties.contains_key("indexd"));
        assert!(atlas.properties.contains_key("noded"));
        assert!(!atlas.properties.contains_key("musicd"));
        apply_reply(
            &RequestKind::PropsGet {
                service: "musicd".into(),
            },
            r#"{"late":true}"#,
            &mut atlas,
            &mut runtime,
        );
        assert!(!atlas.properties.contains_key("musicd"));

        let surface = atlas.properties.get_mut("indexd").unwrap();
        surface.paths = vec!["gone".into(), "kept".into()];
        surface
            .descriptions
            .insert("gone".into(), serde_json::json!({"description":"old"}));
        surface
            .descriptions
            .insert("kept".into(), serde_json::json!({"description":"current"}));
        apply_reply(
            &RequestKind::PropsList {
                service: "indexd".into(),
            },
            r#"["kept"]"#,
            &mut atlas,
            &mut runtime,
        );
        assert_eq!(
            atlas.properties["indexd"]
                .descriptions
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            ["kept"]
        );
    }

    #[test]
    fn failed_inventory_refresh_retains_observation_and_reports_failure() {
        let mut atlas = AtlasState::default();
        assert!(atlas.apply_inventory(
            InventoryProjection::parse(include_str!("fixtures/inventory_verified.json")).unwrap(),
            500,
        ));
        atlas.refreshing = true;
        let mut runtime = AtlasRuntime {
            refresh_failure: Some("inventory unavailable".into()),
            ..AtlasRuntime::default()
        };
        finish_refresh_if_idle(&mut atlas, &mut runtime);
        assert_eq!(atlas.inventory_observed_at_ms, Some(500));
        assert_eq!(
            atlas.notice.as_deref(),
            Some("Mesh refresh failed: inventory unavailable")
        );
    }

    #[test]
    fn citizen_inspection_is_exact_service_and_same_local_node_only() {
        let mut atlas = AtlasState::default();
        assert!(atlas.apply_inventory(
            InventoryProjection::parse(include_str!("fixtures/inventory_verified.json")).unwrap(),
            500,
        ));
        atlas.peers = Some(PeersProjection::parse(r#"{"node":"alpha","peers":[]}"#).unwrap());
        atlas.select("alpha");
        atlas.observe_citizens(
            "alpha",
            vec![crate::model::ServiceInfo {
                name: "studio-bevy-4242".into(),
                binary: Some("cosmix-studio".into()),
                version: Some("0.1.0".into()),
                git_sha: None,
                git_dirty: None,
                pid: Some(4242),
                started_at: None,
            }],
            600,
        );
        assert!(selected_service_is_local(&atlas, "studio-bevy-4242"));
        assert!(!selected_service_is_local(&atlas, "studio"));
        atlas.select("delta");
        assert!(!selected_service_is_local(&atlas, "studio-bevy-4242"));
    }

    #[test]
    fn mutation_key_rejects_second_confirm_while_queued_or_in_flight() {
        let (mut atlas, identity) = selected_local_citizen();
        let mut runtime = AtlasRuntime::default();
        let kind = RequestKind::ActionInvoke {
            service: identity.service.clone(),
            action: "transport.stop".into(),
        };
        let guard = |operation| MutationGuard {
            identity: identity.clone(),
            target: MutationTarget::Action {
                service: identity.service.clone(),
                action: "transport.stop".into(),
            },
            operation,
            invalidated: false,
        };
        assert!(enqueue_mutation(
            &mut runtime,
            atlas.connection_generation,
            identity.service.clone(),
            "action.invoke",
            kind.clone(),
            r#"{"id":"transport.stop"}"#,
            guard(1),
        ));
        atlas.active_mutations.insert(guard(1).target);
        assert!(!enqueue_mutation(
            &mut runtime,
            atlas.connection_generation,
            identity.service.clone(),
            "action.invoke",
            kind.clone(),
            r#"{"id":"transport.stop"}"#,
            guard(2),
        ));
        let call = next_call(&mut runtime, true).unwrap();
        runtime.in_flight.insert(kind.clone());
        runtime.pending.insert(1, call);
        assert!(!enqueue_mutation(
            &mut runtime,
            atlas.connection_generation,
            identity.service.clone(),
            "action.invoke",
            kind,
            r#"{"id":"transport.stop"}"#,
            guard(3),
        ));
        assert!(runtime.reread_needed.is_empty());
    }

    #[test]
    fn queued_mutation_is_purged_when_process_identity_changes() {
        let (mut atlas, identity) = selected_local_citizen();
        let mut runtime = AtlasRuntime::default();
        let kind = RequestKind::ControlSet {
            service: identity.service.clone(),
            control: "transport.toggle".into(),
        };
        let target = MutationTarget::Control {
            service: identity.service.clone(),
            control: "transport.toggle".into(),
        };
        assert!(enqueue_mutation(
            &mut runtime,
            atlas.connection_generation,
            identity.service.clone(),
            "app.controls.set",
            kind.clone(),
            r#"{"target":"transport.toggle","value":null}"#,
            MutationGuard {
                identity: identity.clone(),
                target: target.clone(),
                operation: 1,
                invalidated: false,
            },
        ));
        atlas.active_mutations.insert(target.clone());

        atlas.observe_citizens("alpha", vec![service(&identity.service, 9898)], 700);
        purge_invalid_queued_mutations_inner(&mut atlas, &mut runtime);

        assert!(!runtime.queued.contains_key(&kind));
        assert!(!atlas.active_mutations.contains(&target));
        assert!(atlas
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("not executed")));
    }

    #[test]
    fn in_flight_mutation_reply_is_discarded_after_process_reuse() {
        let (mut atlas, identity) = selected_local_citizen();
        let mut runtime = AtlasRuntime {
            next_operation_id: 2,
            ..AtlasRuntime::default()
        };
        let kind = RequestKind::ControlSet {
            service: identity.service.clone(),
            control: "transport.toggle".into(),
        };
        let target = MutationTarget::Control {
            service: identity.service.clone(),
            control: "transport.toggle".into(),
        };
        assert!(enqueue_mutation(
            &mut runtime,
            atlas.connection_generation,
            identity.service.clone(),
            "app.controls.set",
            kind.clone(),
            r#"{"target":"transport.toggle","value":null}"#,
            MutationGuard {
                identity: identity.clone(),
                target: target.clone(),
                operation: 1,
                invalidated: false,
            },
        ));
        atlas.active_mutations.insert(target.clone());
        let call = next_call(&mut runtime, true).unwrap();
        runtime.in_flight.insert(kind);
        runtime.pending.insert(44, call);

        let replacement = service(&identity.service, 9898);
        let replacement_identity = replacement.process_identity();
        atlas.observe_citizens("alpha", vec![replacement], 700);
        atlas.select_citizen(replacement_identity);
        purge_invalid_queued_mutations_inner(&mut atlas, &mut runtime);
        assert!(runtime.pending[&44]
            .mutation
            .as_ref()
            .is_some_and(|guard| guard.invalidated));
        assert!(atlas.active_mutations.contains(&target));

        handle_reply(
            44,
            Ok(BusReply {
                rc: 0,
                body: r#"{"id":"transport.toggle","value":null}"#.into(),
                result: None,
            }),
            &mut atlas,
            &mut runtime,
        );

        assert!(atlas
            .inspector
            .as_ref()
            .is_some_and(|inspector| inspector.result.is_none()));
        assert!(!runtime.queued.contains_key(&RequestKind::ControlGet {
            service: identity.service,
            control: "transport.toggle".into(),
        }));
        assert!(!atlas.active_mutations.contains(&target));
        assert!(atlas
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("response") && notice.contains("discarded")));
    }

    #[test]
    fn queued_mutation_is_purged_after_citizen_deselection() {
        let (mut atlas, identity) = selected_local_citizen();
        let mut runtime = AtlasRuntime::default();
        let kind = RequestKind::ActionInvoke {
            service: identity.service.clone(),
            action: "transport.toggle".into(),
        };
        let target = MutationTarget::Action {
            service: identity.service.clone(),
            action: "transport.toggle".into(),
        };
        assert!(enqueue_mutation(
            &mut runtime,
            atlas.connection_generation,
            identity.service.clone(),
            "action.invoke",
            kind.clone(),
            r#"{"id":"transport.toggle"}"#,
            MutationGuard {
                identity,
                target: target.clone(),
                operation: 1,
                invalidated: false,
            },
        ));
        atlas.active_mutations.insert(target.clone());
        atlas.selected_citizen = None;
        atlas.inspector = None;

        purge_invalid_queued_mutations_inner(&mut atlas, &mut runtime);

        assert!(!runtime.queued.contains_key(&kind));
        assert!(!atlas.active_mutations.contains(&target));
    }

    #[test]
    fn older_mutation_reply_cannot_replace_latest_result() {
        let identity = ProcessIdentity {
            service: "studio-bevy-4242".into(),
            pid: Some(4242),
            started_at: None,
        };
        let mut atlas = AtlasState {
            selected_citizen: Some("studio-bevy-4242".into()),
            inspector: Some(CitizenInspector::pending(identity)),
            ..AtlasState::default()
        };
        let mut runtime = AtlasRuntime {
            next_operation_id: 3,
            ..AtlasRuntime::default()
        };
        apply_reply_guarded(
            &RequestKind::ActionInvoke {
                service: "studio-bevy-4242".into(),
                action: "old".into(),
            },
            Some(1),
            r#"{"action":"old"}"#,
            &mut atlas,
            &mut runtime,
        );
        assert!(atlas.inspector.as_ref().unwrap().result.is_none());
        apply_reply_guarded(
            &RequestKind::ActionInvoke {
                service: "studio-bevy-4242".into(),
                action: "current".into(),
            },
            Some(2),
            r#"{"action":"current"}"#,
            &mut atlas,
            &mut runtime,
        );
        assert_eq!(
            atlas
                .inspector
                .as_ref()
                .and_then(|inspector| inspector.result.as_ref())
                .map(|result| result.summary.as_str()),
            Some("action.invoke current accepted")
        );
    }

    #[test]
    fn reconnect_queues_a_fresh_observe_start_when_traffic_is_open() {
        let mut runtime = TrafficRuntime::default();
        let mut traffic = TrafficState::default();
        traffic.open = true;
        traffic.subscription_id = Some("pre-reconnect".into());
        traffic.connection_generation = 4;

        handle_observation_connection(BusConnectionState::Connected, 5, &mut traffic, &mut runtime);

        assert_eq!(traffic.subscription_id, None);
        assert!(runtime.desired_start);
    }

    #[test]
    fn pane_close_uses_observation_stop_lane_even_when_atlas_queue_is_full() {
        let mut atlas_runtime = AtlasRuntime::default();
        for index in 0..MAX_QUEUED {
            let kind = RequestKind::NodeInfo {
                node: format!("node-{index}"),
            };
            enqueue_call(
                &mut atlas_runtime,
                1,
                RequestPriority::Remote,
                format!("node-{index}.bus"),
                "noded.info",
                kind,
                String::new(),
            );
        }
        assert_eq!(atlas_runtime.queued.len(), MAX_QUEUED);

        let mut traffic_runtime = TrafficRuntime::default();
        let mut traffic = TrafficState::default();
        traffic.open = true;
        traffic.subscription_id = Some("observe-live".into());
        traffic.observation_connection = BusConnectionState::Connected;
        set_traffic_open(&mut traffic_runtime, &mut traffic, false);

        assert_eq!(
            traffic_runtime.desired_stop.as_deref(),
            Some("observe-live")
        );
        assert!(traffic.status.contains("dedicated observation lane"));
        assert_eq!(atlas_runtime.queued.len(), MAX_QUEUED);
    }
}
