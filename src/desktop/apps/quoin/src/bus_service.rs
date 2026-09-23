use std::collections::{BTreeMap, BTreeSet};

use bevy::ecs::message::MessageWriter;
use bevy::prelude::*;
use bevy::time::Real;
use cosmix_props_core::tree::build_snapshot;
use cosmix_props_core::{PropDescribe, PropPath, PropTree, PropType, PropValue};
use cosmix_shell::core::{Corner, Edge, PanelMode};
use cosmix_shell::runtime::{
    ShellCommand, ShellCommandKind, ShellFrame, ShellFrameState, ShellRuntimeSet,
    ShellSemanticVerb, SubPanelRegistryState, remove_owned_subpanels_before,
    semantic_shell_command,
};
use ctk::app_control::verify_caller_provenance;
use ctk::bus::{BusBridge, BusBridgeEvent, BusConnectionState, BusMessage, InboundRequest};
use serde_json::{Value, json};

use crate::power::{PowerAction, PowerSync};

/// Bound on replies stashed while the outbound channel is full. Beyond this
/// the oldest is dropped with a warning — a bounded stash that eventually
/// answers beats an unbounded one, and both beat silently losing every reply
/// the moment the channel blinks.
const MAX_PENDING_REPLIES: usize = 32;
const RESIZE_RECEIPT_FRAMES: u64 = 120;

#[derive(Component)]
pub(crate) struct QuoinPowerText;

#[derive(Resource)]
struct ShellBusState {
    diagnostics: BusDiagnostics,
    power: PowerSync,
    ready_logged: bool,
    next_request_id: u64,
    /// A snapshot request that could not be queued (outbound channel full).
    /// Re-issued state-drivenly on a later update — no timer — so a
    /// transiently full channel cannot dead-end the display in
    /// "Power unavailable" until an unrelated reconnect.
    snapshot_retry: Option<u64>,
    /// The generation of the last `Connected` event, `None` while down. A
    /// message-triggered resync (`PowerAction::Resync`) is honored only for
    /// this generation: a stale-epoch message drained from the queue after a
    /// reconnect must not start a sync that could land `Ready` on a dead
    /// generation and ignore live telemetry from then on. Refusing costs
    /// nothing — the `Connected` event for the live generation runs its own
    /// sync, and a live-generation change retriggers recovery.
    live_generation: Option<u64>,
    /// Replies that hit a full outbound channel, retried before new inbound
    /// work. Losing a reply outright would leave the peer hanging until its
    /// own timeout — worse than answering late.
    pending_replies: Vec<(InboundRequest, u8, String, Option<ShellCommand>)>,
    pending_resizes: BTreeMap<u64, (InboundRequest, u64)>,
    /// Local receipt ordering, not a broker incarnation token. Absence sweeps
    /// only affect reservations accepted strictly before their cutoff.
    /// CTK uses separate control/telemetry planes: this orders consumption in
    /// Quoin, not the owner's real lifetime across both broker connections.
    citizen_receipt: u64,
    disconnected_citizens: BTreeMap<String, u64>,
    /// Request id, connection generation and conservative acceptance cutoff.
    citizen_snapshot: Option<(u64, u64, u64)>,
    citizen_snapshot_retry: bool,
    frame: u64,
}

impl Default for ShellBusState {
    fn default() -> Self {
        Self {
            diagnostics: BusDiagnostics::default(),
            power: PowerSync::default(),
            ready_logged: false,
            next_request_id: 0x51_0000_0000,
            snapshot_retry: None,
            live_generation: None,
            pending_replies: Vec::new(),
            pending_resizes: BTreeMap::new(),
            citizen_receipt: 0,
            disconnected_citizens: BTreeMap::new(),
            citizen_snapshot: None,
            citizen_snapshot_retry: false,
            frame: 0,
        }
    }
}

#[derive(Default)]
struct BusDiagnostics {
    requests: u64,
    rejected: u64,
    accepted_mutations: u64,
    max_dispatch_us: u64,
}

impl BusDiagnostics {
    fn record(&mut self, rc: u8, mutation: bool, elapsed_us: u64) {
        self.requests = self.requests.saturating_add(1);
        self.rejected = self.rejected.saturating_add(u64::from(rc != 0));
        self.accepted_mutations = self.accepted_mutations.saturating_add(u64::from(mutation));
        self.max_dispatch_us = self.max_dispatch_us.max(elapsed_us);
    }
}

pub(crate) struct ShellBusPlugin;

/// Ordering seam for hosts that prepare the selected output inside the
/// Update schedule: the embedded host replaces the shell model in its
/// `prepare` system, and a seat reserved by a dispatch against the outgoing
/// output must queue its command against the replacement — not against an
/// output the Model stage would drop. Hosts order their output preparation
/// `.before` this set.
#[derive(SystemSet, Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ShellBusDispatch;

impl Plugin for ShellBusPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ShellBusState>()
            .init_resource::<SubPanelRegistryState>()
            .init_resource::<cosmix_scene_bevy::SceneStore>()
            .init_resource::<cosmix_scene_bevy::SceneEvents>()
            .init_resource::<crate::wallpaper::WallpaperState>()
            .init_resource::<crate::demos::DemoState>()
            .init_resource::<crate::config::ShellConfig>()
            .add_message::<cosmix_shell::chrome::QuoinSchemeSelected>()
            .init_resource::<cosmix_shell_host::LayerHostDeadline>()
            .add_message::<cosmix_shell::runtime::ShellResizeResult>()
            .add_systems(
                Update,
                service_bus
                    .in_set(ShellBusDispatch)
                    .in_set(ShellRuntimeSet::Input),
            )
            .add_systems(
                Update,
                apply_citizen_disconnects
                    .in_set(ShellRuntimeSet::Input)
                    .after(service_bus),
            )
            .add_systems(Update, reply_resizes.in_set(ShellRuntimeSet::Presentation));
    }
}

#[derive(bevy::ecs::system::SystemParam)]
struct SceneBus<'w, 's> {
    power_text: Query<'w, 's, &'static mut Text, With<QuoinPowerText>>,
    scenes: ResMut<'w, cosmix_scene_bevy::SceneStore>,
    events: ResMut<'w, cosmix_scene_bevy::SceneEvents>,
    registry: ResMut<'w, SubPanelRegistryState>,
    config: ResMut<'w, crate::config::ShellConfig>,
    schemes: MessageWriter<'w, cosmix_shell::chrome::QuoinSchemeSelected>,
}

// Reply after model application in the same update: a refusal need not
// schedule another frame, and must not wait for an unrelated wake.
fn reply_resizes(
    bridge: Res<BusBridge>,
    mut state: ResMut<ShellBusState>,
    mut results: MessageReader<cosmix_shell::runtime::ShellResizeResult>,
) {
    for result in results.read() {
        if let Some((request, _)) = state.pending_resizes.remove(&result.request_id) {
            let (rc, body) = match &result.result {
                Ok(()) => (0, json!({"accepted":true})),
                Err(cosmix_shell::runtime::ShellResizeError::Configuration(
                    error @ cosmix_shell::core::PanelConfigError::ThicknessBudget {
                        edge,
                        requested,
                        max,
                    },
                )) => (
                    10,
                    json!({"error_code":"PANEL_THICKNESS_BUDGET", "error":error.to_string(), "edge":format!("{edge:?}").to_lowercase(), "requested":requested, "max":max}),
                ),
                Err(cosmix_shell::runtime::ShellResizeError::OutputChanged) => (
                    10,
                    json!({"error_code":"PANEL_OUTPUT_CHANGED", "error":"output geometry changed before the resize applied", "edge":argument(&request, "edge"), "requested":result.requested, "max":result.max}),
                ),
                Err(cosmix_shell::runtime::ShellResizeError::Configuration(error)) => (
                    10,
                    json!({"error_code":"PANEL_RESIZE_REJECTED", "error":error.to_string(), "edge":argument(&request, "edge"), "requested":result.requested, "max":result.max}),
                ),
            };
            stash_or_respond(
                &bridge,
                &mut state,
                request,
                rc,
                body.to_string(),
                None,
                &mut |_| {},
            );
        }
    }
}

fn service_bus(
    bridge: Res<BusBridge>,
    frame: Res<ShellFrameState>,
    time: Res<Time<Real>>,
    mut state: ResMut<ShellBusState>,
    mut shell_commands: MessageWriter<ShellCommand>,
    mut content: SceneBus,
    mut wallpaper: (
        ResMut<crate::wallpaper::WallpaperState>,
        ResMut<cosmix_shell_host::LayerHostDeadline>,
        ResMut<crate::demos::DemoState>,
    ),
) {
    // This system is the app's single inbound drain + reply owner (see
    // `BusBridge::claim_inbound`); Quoin installs no `AppPortPlugin`.
    bridge.claim_inbound("quoin shell service");
    state.frame = state.frame.saturating_add(1);
    let frame_number = state.frame;
    let expired: Vec<_> = state
        .pending_resizes
        .iter()
        .filter_map(|(id, (_, deadline))| (frame_number >= *deadline).then_some(*id))
        .collect();
    for id in expired {
        if let Some((request, _)) = state.pending_resizes.remove(&id) {
            stash_or_respond(
                &bridge,
                &mut state,
                request,
                10,
                json!({"error_code":"PANEL_RESIZE_TIMEOUT", "error":"model receipt expired"})
                    .to_string(),
                None,
                &mut |_| {},
            );
        }
    }

    if let Some(generation) = state.snapshot_retry.take() {
        request_power_snapshot(&bridge, &mut state, generation);
    }
    if state.citizen_snapshot_retry {
        request_citizen_snapshot(&bridge, &mut state);
    }

    let mut power_changed = false;
    for event in bridge.drain_events() {
        content.events.reply(&event);
        wallpaper.0.event(&event, time.elapsed());
        wallpaper.2.event(&event, time.elapsed());
        match event {
            BusBridgeEvent::Connection {
                state: BusConnectionState::Connected,
                generation,
            } => {
                if !state.ready_logged {
                    println!("QUOIN_BUS_READY service={}", bridge.service_name());
                    state.ready_logged = true;
                }
                state.live_generation = Some(generation);
                request_power_snapshot(&bridge, &mut state, generation);
                request_citizen_snapshot(&bridge, &mut state);
                power_changed = true;
            }
            BusBridgeEvent::Connection { .. } | BusBridgeEvent::Fatal(_) => {
                state.pending_resizes.clear();
                state.power.invalidate();
                state.snapshot_retry = None;
                state.live_generation = None;
                state.citizen_snapshot = None;
                state.citizen_snapshot_retry = false;
                state.disconnected_citizens.clear();
                // Replies stashed under the epoch that just ended: the worker
                // drops a response stamped with a stale generation anyway, so
                // retrying them only re-fires dead sends.
                for (_, _, _, command) in state.pending_replies.drain(..) {
                    // Preserve accepted commands even when their reply can
                    // no longer be delivered, including shutdown requests.
                    if let Some(command) = command {
                        shell_commands.write(command);
                    }
                }
                power_changed = true;
            }
            BusBridgeEvent::Reply { request_id, result } => {
                if state
                    .citizen_snapshot
                    .is_some_and(|(id, _, _)| id == request_id)
                {
                    let (_, generation, cutoff) = state.citizen_snapshot.take().unwrap();
                    if state.live_generation == Some(generation) {
                        if let Ok(reply) = result
                            && reply.rc == 0
                            && let Ok(body) = serde_json::from_str::<Value>(&reply.body)
                            && let Some(live) =
                                body.pointer("/services/registered").and_then(service_names)
                        {
                            // A reply may have been captured before a new load.
                            // Use the request's fence, never the later reply time.
                            reconcile_citizens(&mut state, &content.registry.0, &live, cutoff);
                        } else {
                            warn!("citizen registry snapshot failed; awaiting next Bus trigger");
                        }
                    }
                } else {
                    power_changed |= state.power.accept_reply(request_id, result);
                }
            }
            BusBridgeEvent::DroppedMessages(_) => {
                request_citizen_snapshot(&bridge, &mut state);
                if let Some(generation) = state.power.generation() {
                    request_power_snapshot(&bridge, &mut state, generation);
                } else {
                    // No generation to key a sync on; MAJOR-1 recovery kicks
                    // in on the next delivered change instead.
                    state.power.invalidate();
                }
                power_changed = true;
            }
            BusBridgeEvent::ObservationDroppedMessages(_) => {
                request_citizen_snapshot(&bridge, &mut state);
            }
            BusBridgeEvent::ObservationConnection { .. }
            | BusBridgeEvent::ObservationReply { .. } => {}
        }
    }
    for message in bridge.drain_messages() {
        wallpaper.0.message(&message, time.elapsed());
        if state.live_generation == Some(message.connection_generation) {
            if let Some(live) = registered_services(&message) {
                state.citizen_receipt = state
                    .citizen_receipt
                    .checked_add(1)
                    .expect("receipt sequence exhausted");
                let cutoff = state.citizen_receipt;
                // This full observation supersedes any in-flight snapshot.
                state.citizen_snapshot = None;
                state.citizen_snapshot_retry = false;
                reconcile_citizens(&mut state, &content.registry.0, &live, cutoff);
            } else if message
                .headers
                .get("gap")
                .is_some_and(|value| value == "true")
            {
                request_citizen_snapshot(&bridge, &mut state);
            }
        }
        match state.power.observe_message(message) {
            PowerAction::None => {}
            PowerAction::Changed => power_changed = true,
            PowerAction::Resync { generation } => {
                // Only the live generation may start a sync (see
                // `live_generation`); a refused stale trigger recovers via
                // the Connected event or the next live-generation change.
                if state.live_generation == Some(generation) {
                    request_power_snapshot(&bridge, &mut state, generation);
                    power_changed = true;
                }
            }
        }
    }
    wallpaper.0.tick(&bridge, time.elapsed(), &mut wallpaper.1);
    wallpaper.2.tick(&bridge, time.elapsed(), &mut wallpaper.1);
    if power_changed {
        let rendered = state.power.render();
        for mut text in &mut content.power_text {
            **text = rendered.clone();
        }
    }

    // Retry stashed replies before answering new work so a recovered channel
    // drains in arrival order.
    let pending = std::mem::take(&mut state.pending_replies);
    let mut dispatch = |command| {
        shell_commands.write(command);
    };
    for (request, rc, body, command) in pending {
        stash_or_respond(
            &bridge,
            &mut state,
            request,
            rc,
            body,
            command,
            &mut dispatch,
        );
    }

    for request in bridge.drain_inbound() {
        let started = std::time::Instant::now();
        let (rc, body, command) = if let Some(verb) =
            cosmix_shell::runtime::SceneVerb::parse(&request.command)
        {
            let args = parse_args(&request).unwrap_or(Value::Null);
            let (rc, body) = if let Err(error) = verify_caller_provenance(&request) {
                (
                    10,
                    json!({"error":format!("scene caller provenance: {error:?}")}).to_string(),
                )
            } else if state
                .live_generation
                .is_some_and(|generation| generation != request.connection_generation)
            {
                (
                    10,
                    json!({"error":"scene request belongs to a stale Quoin connection"})
                        .to_string(),
                )
            } else {
                state.citizen_receipt = state
                    .citizen_receipt
                    .checked_add(1)
                    .expect("receipt sequence exhausted");
                let owner = attested_owner(&request, state.citizen_receipt);
                let SceneBus {
                    scenes, registry, ..
                } = &mut content;
                scenes.dispatch(
                    verb,
                    &request.body,
                    &args,
                    &bridge,
                    &mut cosmix_scene_bevy::SceneMount {
                        registry: &mut registry.0,
                        output: &frame.0.geometry.output,
                        owner: &owner,
                        accepted_at: state.citizen_receipt,
                    },
                )
            };
            (rc, body, None)
        } else if matches!(
            request.command.as_str(),
            "shell.sub.register" | "shell.sub.remove"
        ) {
            let (rc, body, command) = dispatch_sub_panel_verb(
                &request,
                &frame.0,
                &mut content.registry.0,
                &mut state,
                time.elapsed(),
            );
            (rc, body, command)
        } else if request.command.starts_with("shell.settings.") {
            // The bridge drops stale epochs before dispatch; the same fence
            // the scene and sub-panel verbs keep — a stale request must not
            // spend a settings write (a live theme application, a conf.mix
            // rewrite or a resize command).
            if state
                .live_generation
                .is_some_and(|generation| generation != request.connection_generation)
            {
                (
                    10,
                    json!({"error":"settings request belongs to a stale Quoin connection"})
                        .to_string(),
                    None,
                )
            } else {
                let SceneBus {
                    config, schemes, ..
                } = &mut content;
                crate::settings::dispatch_verb(
                    &request,
                    &frame.0,
                    config,
                    &crate::config::conf_mix_path(),
                    schemes,
                    time.elapsed(),
                )
            }
        } else if request.command == "shell.debug.status" {
            (
                0,
                json!({
                    "requests":state.diagnostics.requests,
                    "rejected":state.diagnostics.rejected,
                    "accepted_mutations":state.diagnostics.accepted_mutations,
                    "max_dispatch_us":state.diagnostics.max_dispatch_us,
                    "pending_replies":state.pending_replies.len(),
                    "connected":state.live_generation.is_some(),
                    "scope":"this process; dispatch excludes model application and transport"
                })
                .to_string(),
                None,
            )
        } else {
            dispatch_shell_request(&request, &frame.0, time.elapsed())
        };
        let elapsed_us = started.elapsed().as_micros().min(u64::MAX as u128) as u64;
        state.diagnostics.record(rc, command.is_some(), elapsed_us);
        if command.is_some() || rc != 0 {
            bevy::log::debug!(
                command = request.command.as_str(),
                rc,
                dispatch_us = elapsed_us,
                accepted_mutation = command.is_some(),
                pending_replies = state.pending_replies.len(),
                "QUOIN_BUS_DISPATCH"
            );
        }
        // A snapshot check can become stale behind another queued command.
        // Only the model's application receipt may acknowledge a resize.
        if let Some(ShellCommand {
            output,
            at,
            kind: ShellCommandKind::ResizeCommit { edge, thickness_px },
        }) = &command
        {
            if state.pending_resizes.len() < MAX_PENDING_REPLIES {
                state.next_request_id = state.next_request_id.saturating_add(1);
                let request_id = state.next_request_id;
                let deadline = state.frame.saturating_add(RESIZE_RECEIPT_FRAMES);
                state
                    .pending_resizes
                    .insert(request_id, (request, deadline));
                dispatch(ShellCommand {
                    output: output.clone(),
                    at: *at,
                    kind: ShellCommandKind::ResizeChecked {
                        edge: *edge,
                        thickness_px: *thickness_px,
                        request_id,
                    },
                });
            } else {
                stash_or_respond(
                    &bridge,
                    &mut state,
                    request,
                    11,
                    json!({"error":"resize queue full"}).to_string(),
                    None,
                    &mut dispatch,
                );
            }
            continue;
        }
        stash_or_respond(
            &bridge,
            &mut state,
            request,
            rc,
            body,
            command,
            &mut dispatch,
        );
    }
}

/// The full registration set is authoritative; `old` is deliberately ignored.
/// A missed diff is repaired by the next full observation, reconnect or gap.
fn registered_services(message: &BusMessage) -> Option<BTreeSet<String>> {
    if message.topic() != Some("noded.props.changed") {
        return None;
    }
    let body = serde_json::from_str::<Value>(&message.body).ok()?;
    if body["path"] != "services.registered" {
        return None;
    }
    service_names(&body["new"])
}

fn service_names(value: &Value) -> Option<BTreeSet<String>> {
    // A partial/malformed list is not evidence that an owner disappeared.
    value
        .as_array()?
        .iter()
        .map(|name| name.as_str().map(str::to_owned))
        .collect()
}

fn reconcile_citizens(
    state: &mut ShellBusState,
    registry: &cosmix_shell::core::SubPanelRegistry,
    live: &BTreeSet<String>,
    cutoff: u64,
) {
    for owner in registry.live_owners().difference(live) {
        state
            .disconnected_citizens
            .entry(owner.clone())
            .and_modify(|before| *before = (*before).max(cutoff))
            .or_insert(cutoff);
    }
}

/// A registered local sender is broker-restamped `from`. Attested mesh
/// identity is qualified so a remote service cannot alias a local owner.
/// Anonymous callers remain mesh-open, but have no discoverable lifetime or
/// stable identity for same-owner updates. Give each acceptance a distinct
/// untracked seat owner instead of conflating unrelated anonymous callers.
///
/// The one owner-derivation rule for every citizen-facing ingress (scene
/// mounts and the sub-panel verbs alike): caller-authored metadata or verb
/// arguments never choose lifetime ownership.
fn attested_owner(request: &InboundRequest, receipt: u64) -> String {
    if let (Some(peer), Some(service)) = (
        request.headers.get("broker_peer"),
        request.headers.get("broker_service"),
    ) {
        return format!("{service}@{peer}");
    }
    if request.from.is_empty() {
        format!("anonymous@{receipt}")
    } else {
        request.from.clone()
    }
}

/// Event-driven resync only. A full outbound queue retries this one request
/// on the next update; a failed RPC waits for the next observation/gap/connect.
fn request_citizen_snapshot(bridge: &BusBridge, state: &mut ShellBusState) {
    state.citizen_snapshot_retry = false;
    let Some(generation) = state.live_generation else {
        return;
    };
    state.next_request_id = state.next_request_id.saturating_add(1);
    let id = state.next_request_id;
    state.citizen_receipt = state
        .citizen_receipt
        .checked_add(1)
        .expect("receipt sequence exhausted");
    state.citizen_snapshot = Some((id, generation, state.citizen_receipt));
    if bridge
        .try_call(id, "noded", "noded.props.get", BTreeMap::new(), "{}")
        .is_err()
    {
        state.citizen_snapshot = None;
        state.citizen_snapshot_retry = !bridge.worker_is_gone();
    }
}

/// Apply broker-reported citizen disconnects with world access: for each
/// owner, older sub-panel seats and their carousel content are removed first
/// (landing per the carousel's removal rule), then its scenes unload so the
/// scene reconcile in this frame destroys the mounted content without
/// rebuilding the carousel or changing its remembered selection.
fn apply_citizen_disconnects(world: &mut World) {
    let citizens = std::mem::take(&mut world.resource_mut::<ShellBusState>().disconnected_citizens);
    for (citizen, before) in &citizens {
        remove_owned_subpanels_before(world, citizen, *before);
        if let Some(mut store) = world.get_resource_mut::<cosmix_scene_bevy::SceneStore>() {
            let scenes = store.unload_owned_before(citizen, *before);
            if !scenes.is_empty() {
                println!(
                    "QUOIN_CITIZEN_DISCONNECT citizen={citizen} scenes={}",
                    scenes.join(",")
                );
            }
        }
    }
}

/// Answer, or stash for a state-driven retry when the outbound channel is
/// full. A dropped reply leaves the peer hanging until its own timeout.
///
/// A send failure is only worth retrying when the channel is FULL. When the
/// worker is GONE nothing will ever drain it, and — because the worker owned
/// the sending end of the event channel too — no `Fatal`/`Connection` event
/// can arrive to clear the stash either, so a stashed reply would re-fire a
/// dead `try_send` every frame for the life of the process. Drop it loudly
/// instead; the peer's own timeout is the honest outcome.
/// Defer the command until its reply is queued, unless the worker is gone:
/// then dispatch despite the lost reply so shutdown can still complete.
fn stash_or_respond(
    bridge: &BusBridge,
    state: &mut ShellBusState,
    request: InboundRequest,
    rc: u8,
    body: String,
    command: Option<ShellCommand>,
    dispatch: &mut impl FnMut(ShellCommand),
) {
    if let Err(error) = bridge.try_respond(&request, rc, body.clone()) {
        if bridge.worker_is_gone() {
            bevy::log::warn!(
                command = request.command.as_str(),
                "shell Bus worker has stopped; dropping reply ({error})"
            );
            if let Some(command) = command {
                dispatch(command);
            }
            return;
        }
        if state.pending_replies.len() >= MAX_PENDING_REPLIES {
            let (dropped, ..) = state.pending_replies.remove(0);
            bevy::log::warn!(
                command = dropped.command.as_str(),
                "shell Bus reply stash full; dropping oldest pending reply"
            );
        }
        bevy::log::warn!("shell Bus response deferred: {error}");
        state.pending_replies.push((request, rc, body, command));
    } else if let Some(command) = command {
        dispatch(command);
    }
}

fn request_power_snapshot(bridge: &BusBridge, state: &mut ShellBusState, generation: u64) {
    state.snapshot_retry = None;
    state.next_request_id = state.next_request_id.saturating_add(1);
    let request_id = state.next_request_id;
    state.power.begin(generation, request_id);
    if bridge
        .try_call(
            request_id,
            "power",
            "power.props.get",
            BTreeMap::new(),
            "{}",
        )
        .is_err()
    {
        if bridge.worker_is_gone() {
            // No retry can ever succeed, and no Fatal event will arrive to
            // clear one — the events channel died with the worker. Settle on
            // Unavailable (rendered honestly as "Power unavailable") and stop
            // asking, rather than re-firing a dead send every frame forever.
            state.power.invalidate();
            state.live_generation = None;
            return;
        }
        // Outbound channel merely full. Stay Syncing — also rendered as
        // "Power unavailable" — and re-issue on a later update instead of
        // dead-ending until an unrelated reconnect.
        state.snapshot_retry = Some(generation);
    }
}

/// The sub-panel lifecycle verbs (panel doc §3): `sub.register` and
/// `sub.remove` — activation is a later, separate verb.
///
/// These validate against the process-wide registry, not just the frame: a
/// name is globally unique across all four edges and all outputs, and only
/// the registry knows names outside the selected model. Correctness checks
/// alone refuse (duplicate name, unknown name, stale generation) — the
/// mesh's full-verb-access law applies, so there is deliberately no "who
/// may" gate beyond the provenance stamp every verb requires.
///
/// Registering reserves the seat transactionally at dispatch,
/// receipt-stamped exactly like a scene mount, so two same-name
/// registrations drained in one batch cannot both be acked; the enqueued
/// command fills the carousel at the Model stage of this update. Removing
/// resolves the seat here and carries its owner and acceptance receipt in
/// the command — the registry applies the carousel's removal landing rule
/// at the Model stage, atomically with the seat, and only while that exact
/// registration still stands.
fn dispatch_sub_panel_verb(
    request: &InboundRequest,
    frame: &ShellFrame,
    registry: &mut cosmix_shell::core::SubPanelRegistry,
    state: &mut ShellBusState,
    at: std::time::Duration,
) -> (u8, String, Option<ShellCommand>) {
    if let Err(error) = verify_caller_provenance(request) {
        return (
            10,
            json!({"error":format!("sub-panel caller provenance: {error:?}")}).to_string(),
            None,
        );
    }
    // The bridge drops stale epochs before dispatch; this is the same fence
    // the scene verbs keep — a stale request must not spend a seat.
    if state
        .live_generation
        .is_some_and(|generation| generation != request.connection_generation)
    {
        return (
            10,
            json!({"error":"sub-panel request belongs to a stale Quoin connection"}).to_string(),
            None,
        );
    }
    let register = request.command == "shell.sub.register";
    let verb = if register { "register" } else { "remove" };
    let Some(name) = argument(request, "name").filter(|name| !name.trim().is_empty()) else {
        return (
            10,
            json!({"error":format!("sub.{verb} requires a name argument")}).to_string(),
            None,
        );
    };
    if register {
        let Some(edge) = argument(request, "edge").and_then(parse_edge) else {
            return (
                10,
                json!({"error":"edge must be left, bottom, right or top"}).to_string(),
                None,
            );
        };
        state.citizen_receipt = state
            .citizen_receipt
            .checked_add(1)
            .expect("receipt sequence exhausted");
        let receipt = state.citizen_receipt;
        let owner = attested_owner(request, receipt);
        return register_sub_panel(frame, registry, name, edge, owner, receipt, at);
    }
    // Removal: the name is the address; the seat supplies the edge, owner
    // and acceptance receipt the command carries. An unknown name is
    // refused, never a creation. The Model stage applies the removal only
    // while that exact registration (same owner AND same receipt) still
    // stands, so a replacement accepted after this seat was dropped
    // survives the stale command.
    let Some((edge, owner, accepted_at)) = registry
        .seat(&name)
        .map(|seat| (seat.edge, seat.owner.clone(), seat.accepted_at))
    else {
        let error = cosmix_shell::core::SubPanelRegistryError::Unknown(name);
        return (10, json!({"error":error.to_string()}).to_string(), None);
    };
    let command = semantic_shell_command(
        frame.geometry.output.clone(),
        at,
        edge,
        ShellSemanticVerb::SubRemove {
            name,
            owner,
            accepted_at,
        },
    );
    (0, json!({"accepted":true}).to_string(), Some(command))
}

/// Shared sub.register acceptance path for Bus callers and host-owned content.
/// The caller supplies its attested owner; registration never reveals a panel.
pub(crate) fn register_sub_panel(
    frame: &ShellFrame,
    registry: &mut cosmix_shell::core::SubPanelRegistry,
    name: String,
    edge: Edge,
    owner: String,
    receipt: u64,
    at: std::time::Duration,
) -> (u8, String, Option<ShellCommand>) {
    // Global duplicate first (the registry is the address space), then
    // the frame: a live page without a seat (host chrome content) is as
    // taken as a seated one, on ANY edge of this output — names are
    // globally unique, so a seat-less page on another edge refuses a
    // registration the requested edge alone would have accepted.
    if registry.seat(&name).is_some() {
        let error = cosmix_shell::core::SubPanelRegistryError::Duplicate(name);
        return (10, json!({"error":error.to_string()}).to_string(), None);
    }
    if Edge::ALL.into_iter().any(|live_edge| {
        frame
            .panel(live_edge)
            .page_ids
            .iter()
            .any(|page| page == &name)
    }) {
        return (
            10,
            json!({"error":format!("name '{name}' is already a page on this output")}).to_string(),
            None,
        );
    }
    if let Err(error) = registry.mount(&name, frame.geometry.output.clone(), edge, &owner, receipt)
    {
        return (10, json!({"error":error.to_string()}).to_string(), None);
    }
    let command = semantic_shell_command(
        frame.geometry.output.clone(),
        at,
        edge,
        ShellSemanticVerb::SubRegister { name, owner },
    );
    (0, json!({"accepted":true}).to_string(), Some(command))
}

fn dispatch_shell_request(
    request: &InboundRequest,
    frame: &ShellFrame,
    at: std::time::Duration,
) -> (u8, String, Option<ShellCommand>) {
    if request.command == "shell.ping" {
        return (
            0,
            json!({"service":"shell","status":"ok"}).to_string(),
            None,
        );
    }
    if request.command == "shell.info" {
        return (0, json!({
            "service":"shell",
            "contract":"cosmix-shell.v1",
            "props":["get","list","describe"],
            "verbs":["quit","panel.show","panel.hide","panel.toggle","panel.pin","panel.unpin","panel.dock","panel.mode","panel.resize","panel.page.next","panel.page.prev","panel.page.set","sub.register","sub.remove","settings.scheme","settings.motion","settings.size","corner.show","corner.hide","corner.toggle","corner.pin","corner.unpin","debug.status","scene.load","scene.patch","scene.get","scene.describe","scene.unload","scene.watch"],
            "corners":{"top-left":"left","bottom-left":"bottom","bottom-right":"right","top-right":"top"}
        }).to_string(), None);
    }
    // The transport admits `app.*`/`action.*` on every inbound port; this
    // service does not implement that contract. Answer with the real reason,
    // not a routing-confusion "unknown shell command".
    if request.command.starts_with("app.") || request.command.starts_with("action") {
        return (
            10,
            json!({"error":"app and action verbs are not supported by service shell"}).to_string(),
            None,
        );
    }
    if let Some(suffix) = request.command.strip_prefix("shell.props.") {
        let args = parse_args(request);
        let response = cosmix_props_core::bus::dispatch_props(
            &ShellProps(frame),
            suffix,
            args.as_ref(),
            false,
        );
        return (
            response.rc.clamp(0, u8::MAX as i32) as u8,
            response.body,
            None,
        );
    }
    if request.command == "shell.quit" {
        if let Err(error) = verify_caller_provenance(request) {
            return (
                10,
                json!({"error":format!("caller provenance could not be established: {error:?}")})
                    .to_string(),
                None,
            );
        }
        return (
            0,
            json!({"accepted":true}).to_string(),
            Some(ShellCommand {
                output: frame.geometry.output.clone(),
                at,
                kind: ShellCommandKind::Quit,
            }),
        );
    }
    if request.command == "shell.panel.resize" {
        if let Err(error) = verify_caller_provenance(request) {
            return (
                10,
                json!({"error":format!("caller provenance could not be established: {error:?}")})
                    .to_string(),
                None,
            );
        }
        let Some(edge) = argument(request, "edge").and_then(parse_edge) else {
            return (
                10,
                json!({"error":"edge must be left, bottom, right or top"}).to_string(),
                None,
            );
        };
        let Some(thickness_px) = number_argument(request, "thickness_px")
            .map(|value| value as f32)
            .filter(|value| cosmix_shell::core::RESIZE_THICKNESS_RANGE.contains(value))
        else {
            return (
                10,
                json!({"error":"thickness_px must be a number in 120..=500"}).to_string(),
                None,
            );
        };
        let max = frame.panel(edge).max_thickness_px;
        if thickness_px > max {
            return (
                10,
                json!({"error_code":"PANEL_THICKNESS_BUDGET", "error":"panel thickness exceeds output budget", "edge":argument(request, "edge"), "requested":thickness_px, "max":max}).to_string(),
                None,
            );
        }
        return (
            0,
            json!({"accepted":true}).to_string(),
            Some(ShellCommand {
                output: frame.geometry.output.clone(),
                at,
                kind: ShellCommandKind::ResizeCommit { edge, thickness_px },
            }),
        );
    }
    if request.command == "shell.panel.page.set" && argument(request, "id").is_none() {
        return (
            10,
            json!({"error":"page.set requires an id argument"}).to_string(),
            None,
        );
    }
    // Validate the precise mode verb's argument before `semantic_verb`, which
    // would otherwise collapse a bad mode into "unknown shell command".
    if request.command == "shell.panel.mode" {
        match argument(request, "mode") {
            None => {
                return (
                    10,
                    json!({"error":"panel.mode requires a mode argument"}).to_string(),
                    None,
                );
            }
            Some(value) if PanelMode::parse(&value).is_none() => {
                return (
                    10,
                    json!({"error":"mode must be hidden, pinned or docked"}).to_string(),
                    None,
                );
            }
            Some(_) => {}
        }
    }
    let Some(verb) = semantic_verb(request) else {
        return (
            10,
            json!({"error":"unknown shell command"}).to_string(),
            None,
        );
    };
    // The PROVENANCE gate — no longer a "who may" one (TODO-cos, filed
    // 2026-09-20; full-mesh-access law, 2026-09-15).
    //
    // It used to require local callers to hold a registered service identity,
    // which made `send "shell" shell.panel.pin edge="right"` from a plain
    // `mix -c` fail with `UnregisteredCaller` — locking out the agentless
    // one-shot, which is the sovereign control path, while the panel citizen
    // beside it worked fine. That is a "who may" gate by the law's own test.
    //
    // It also protected nothing, though NOT for the reason first recorded
    // here. "Anyone on the local Bus could signal this process anyway" is
    // false — the ingress socket is world read-write, so a different-uid or
    // kill-sandboxed caller can reach the Bus without being able to signal
    // anything. The true reason is that the check was lexical: it tested the
    // SHAPE of `from`, and same-node registration is itself ungated, so
    // anything that wanted to pass it registered a two-character name and
    // did. See `ctk::app_control::verify_caller_provenance` for the full
    // argument.
    //
    // CROSS-COMPONENT TRUST DEPENDENCY, unchanged and still load-bearing:
    // what remains is only as strong as noded's guarantee to strip
    // client-supplied `broker_origin`/identity headers and restamp them from
    // connection state. On the LOCAL lane a self-asserted
    // `source_peer`/`permissions`/`signed_ident` is refused, as is a missing
    // stamp or a duplicated one, because each says the stamp cannot be
    // trusted — not that the caller is the wrong one. Absence fails closed.
    // The MESH lane returns before that identity check, deliberately, so a
    // mesh-stamped frame carrying `signed_ident` is admitted; nothing here
    // grants authority from the header. The correctness checks below (edge
    // valid, page id known on that edge) are what decide whether the
    // operation is well formed and aimed correctly, and they stay.
    if let Err(error) = verify_caller_provenance(request) {
        return (
            10,
            json!({"error":format!("caller provenance could not be established: {error:?}")})
                .to_string(),
            None,
        );
    }
    let corner_command = request.command.starts_with("shell.corner.");
    let selected_edge = if corner_command {
        argument(request, "corner").and_then(|value| match value.as_str() {
            "top-left" => Some(Corner::TopLeft.summoned_edge()),
            "bottom-left" => Some(Corner::BottomLeft.summoned_edge()),
            "bottom-right" => Some(Corner::BottomRight.summoned_edge()),
            "top-right" => Some(Corner::TopRight.summoned_edge()),
            _ => None,
        })
    } else {
        argument(request, "edge").and_then(parse_edge)
    };
    let Some(edge) = selected_edge else {
        return (
            10,
            json!({"error":if corner_command {
                "corner must be top-left, bottom-left, bottom-right or top-right"
            } else { "edge must be left, bottom, right or top" }})
            .to_string(),
            None,
        );
    };
    // Refuse an unknown page id here, against the current frame: the Model
    // stage silently drops carousel errors, so acking it would report a
    // mutation that will never happen.
    if let ShellSemanticVerb::PageSet(ref id) = verb
        && !frame.panel(edge).page_ids.iter().any(|page| page == id)
    {
        return (
            10,
            json!({"error":"unknown page id for this edge"}).to_string(),
            None,
        );
    }
    let command = semantic_shell_command(frame.geometry.output.clone(), at, edge, verb);
    // `accepted` means validated and enqueued for the Model stage of this
    // update — an acceptance ack, not an application receipt. Callers needing
    // the applied state read it back via `shell.props.get`.
    (0, json!({"accepted":true}).to_string(), Some(command))
}

fn semantic_verb(request: &InboundRequest) -> Option<ShellSemanticVerb> {
    Some(match request.command.as_str() {
        "shell.panel.show" | "shell.corner.show" => ShellSemanticVerb::PanelShow,
        "shell.panel.hide" | "shell.corner.hide" => ShellSemanticVerb::PanelHide,
        "shell.panel.toggle" | "shell.corner.toggle" => ShellSemanticVerb::PanelToggle,
        "shell.panel.pin" | "shell.corner.pin" => ShellSemanticVerb::PanelPin,
        "shell.panel.unpin" | "shell.corner.unpin" => ShellSemanticVerb::PanelUnpin,
        "shell.panel.dock" => ShellSemanticVerb::PanelDock,
        "shell.panel.mode" => {
            ShellSemanticVerb::PanelMode(PanelMode::parse(&argument(request, "mode")?)?)
        }
        "shell.panel.page.next" => ShellSemanticVerb::PageNext,
        "shell.panel.page.prev" => ShellSemanticVerb::PagePrevious,
        "shell.panel.page.set" => ShellSemanticVerb::PageSet(argument(request, "id")?),
        _ => return None,
    })
}

fn parse_args(request: &InboundRequest) -> Option<Value> {
    serde_json::from_str(&request.body).ok()
}

/// Bus wire headers the TRANSPORT owns, never the caller.
///
/// `InboundRequest::headers` is documented as carrying *every* header off the
/// wire, and the broker stamps this core-protocol block itself — `id` is the
/// request's correlation id (`cosmix_lib_client`'s `call_typed` assigns it a
/// monotonic counter), `from`/`to`/`command`/`type` are routing. Reading a
/// verb argument out of one of these names does not read the caller's word
/// for it; it reads the transport's, silently.
///
/// That is not hypothetical: `shell.panel.page.set … id=<page>` sent through
/// Mix's ordinary JSON-body RPC put the page id in the BODY while the broker
/// put its correlation id in the `id` HEADER, and a header-first lookup
/// validated the correlation id as a page id — so every well-formed
/// `page.set` was refused with "unknown page id for this edge" while
/// `shell.props.get panels.<edge>.pages` (which reads the body only)
/// advertised that exact page. The list mirrors the core-protocol block of
/// `cosmix_lib_bus::KNOWN_HEADERS`; the display-protocol names in that
/// constant are NOT transport-owned and stay addressable.
///
/// Deliberately not a header-stripping change in the transport: `InboundRequest`
/// hands the app the wire verbatim on purpose (`broker_origin` and
/// `signed_ident` are read straight off it by [`verify_caller_provenance`]), so
/// the rule belongs at the one place that maps headers to caller arguments.
const WIRE_OWNED_HEADERS: &[&str] = &[
    "bus",
    "type",
    "id",
    "from",
    "to",
    "command",
    "args",
    "json",
    "reply-to",
    "ttl",
    "error",
    "timestamp",
    "rc",
];

/// A caller-supplied argument, from the JSON body or a non-transport header.
///
/// Header routing (Mix's `body=` shape) and JSON-body RPC are both live
/// callers, so both sources are read; a name the transport owns is read from
/// the body ONLY, because a header of that name is the broker's value and
/// never the caller's. Under header routing such an argument is therefore
/// simply absent — the honest answer, and the one that makes `page.set`
/// report "requires an id argument" instead of rejecting the broker's
/// correlation id as an unknown page.
pub(crate) fn argument(request: &InboundRequest, name: &str) -> Option<String> {
    if !WIRE_OWNED_HEADERS
        .iter()
        .any(|owned| owned.eq_ignore_ascii_case(name))
        && let Some(value) = request.headers.get(name)
    {
        return Some(value.clone());
    }
    parse_args(request)?.get(name)?.as_str().map(str::to_owned)
}

/// Read a finite numeric argument, accepting both a JSON number
/// (`thickness_px=240`) and a numeric string (`thickness_px="240"`) — Mix's
/// `send … k=v` may deliver either shape.
pub(crate) fn number_argument(request: &InboundRequest, name: &str) -> Option<f64> {
    let value = parse_args(request)?;
    let field = value.get(name)?;
    let number = field
        .as_f64()
        .or_else(|| field.as_str().and_then(|text| text.parse::<f64>().ok()))?;
    number.is_finite().then_some(number)
}

pub(crate) fn parse_edge(value: String) -> Option<Edge> {
    match value.as_str() {
        "left" => Some(Edge::Left),
        "bottom" => Some(Edge::Bottom),
        "right" => Some(Edge::Right),
        "top" => Some(Edge::Top),
        _ => None,
    }
}

struct ShellProps<'a>(&'a ShellFrame);

impl PropTree for ShellProps<'_> {
    fn snapshot(&self) -> PropValue {
        let mut leaves = Vec::new();
        for edge in Edge::ALL {
            let name = edge_name(edge);
            let panel = self.0.panel(edge);
            leaves.extend([
                leaf(format!("panels.{name}.visible"), panel.mapped.into()),
                leaf(
                    format!("panels.{name}.pinned"),
                    // Compatibility shim, not a precise mode signal. Transient
                    // visibility of a Hidden panel must never read as pinned.
                    (panel.mode != PanelMode::Hidden).into(),
                ),
                // The precise signal: the panel's persistent mode, independent
                // of transient visibility.
                leaf(format!("panels.{name}.mode"), panel.mode.as_str().into()),
                leaf(
                    format!("panels.{name}.width_px"),
                    (panel.thickness_px as f64).into(),
                ),
                leaf(
                    format!("panels.{name}.page"),
                    panel
                        .active_page_id
                        .clone()
                        .map_or(PropValue::Null, PropValue::from),
                ),
                leaf(
                    format!("panels.{name}.pages"),
                    panel.page_ids.iter().cloned().collect::<Vec<_>>().into(),
                ),
                leaf(
                    format!("panels.{name}.output"),
                    self.0.geometry.output.as_str().into(),
                ),
            ]);
        }
        build_snapshot(leaves)
    }

    fn list(&self) -> Vec<PropPath> {
        let mut paths = Vec::new();
        for edge in Edge::ALL {
            for field in ["visible", "pinned", "mode", "width_px", "page", "pages", "output"] {
                paths.push(PropPath::new(format!("panels.{}.{}", edge_name(edge), field)).unwrap());
            }
        }
        paths
    }

    fn describe(&self, path: &PropPath) -> Option<PropDescribe> {
        let field = path.as_str().rsplit('.').next()?;
        let ty = match field {
            "visible" | "pinned" => PropType::Bool,
            "mode" | "page" | "output" => PropType::String,
            "width_px" => PropType::Number,
            "pages" => PropType::List,
            _ => return None,
        };
        Some(PropDescribe::leaf(
            path.clone(),
            ty,
            match field {
                "pinned" => {
                    "compatibility shim: true for persistent Pinned or Docked; false for Hidden, including transient reveal"
                }
                "mode" => {
                    "persistent panel mode: hidden, pinned (overlay, reserves nothing) or docked (reserves its thickness)"
                }
                _ => "live Quoin panel state",
            },
        ))
    }
}

fn leaf(path: String, value: PropValue) -> (PropPath, PropValue) {
    (
        PropPath::new(path).expect("static shell property path"),
        value,
    )
}

fn edge_name(edge: Edge) -> &'static str {
    match edge {
        Edge::Left => "left",
        Edge::Bottom => "bottom",
        Edge::Right => "right",
        Edge::Top => "top",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_shell::core::PanelInput;
    use cosmix_shell::runtime::{CarouselInput, ShellCommandKind};
    use ctk::bus::test_bridge;

    fn request(command: &str) -> InboundRequest {
        InboundRequest {
            connection_generation: 1,
            from: "peer".to_owned(),
            command: command.to_owned(),
            headers: BTreeMap::new(),
            body: r#"{"edge":"left"}"#.to_owned(),
            reply_id: Some("1".to_owned()),
        }
    }

    fn local(command: &str) -> InboundRequest {
        let mut request = request(command);
        request
            .headers
            .insert("broker_origin".to_owned(), "local".to_owned());
        request
    }

    #[test]
    fn quit_requires_broker_stamped_local_and_only_enqueues() {
        let frame = test_frame();
        let (rc, _, command) =
            dispatch_shell_request(&request("shell.quit"), &frame, std::time::Duration::ZERO);
        assert_eq!(rc, 10);
        assert!(command.is_none());
        let mut request = local("shell.quit");
        request.body = "{}".into();
        let (rc, body, command) =
            dispatch_shell_request(&request, &frame, std::time::Duration::ZERO);
        assert_eq!(rc, 0);
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap(),
            json!({"accepted":true})
        );
        assert_eq!(command.unwrap().kind, ShellCommandKind::Quit);
        request
            .headers
            .insert("broker_origin".into(), "mesh".into());
        assert_eq!(
            dispatch_shell_request(&request, &frame, std::time::Duration::ZERO).0,
            0
        );
    }

    /// `shell.info` is the discovery surface: a verb missing from its list is
    /// a verb no script can find, so the settings verbs must be advertised.
    #[test]
    fn shell_info_lists_the_settings_verbs() {
        let frame = test_frame();
        let (rc, body, _) =
            dispatch_shell_request(&request("shell.info"), &frame, Default::default());
        assert_eq!(rc, 0);
        let info: Value = serde_json::from_str(&body).unwrap();
        let verbs = info["verbs"].as_array().expect("verbs is a list");
        for verb in ["settings.scheme", "settings.motion", "settings.size"] {
            assert!(
                verbs.contains(&json!(verb)),
                "shell.info must advertise {verb}; got {verbs:?}"
            );
        }
    }

    /// The filed defect (TODO-cos, 2026-09-20): `send "shell"
    /// shell.panel.pin edge="right"` from a plain `mix -c` answered
    /// `local registered caller required: UnregisteredCaller`, so the
    /// agentless one-shot — the sovereign control path — was the one caller
    /// that could not drive the panel, while the registered panel citizen
    /// beside it worked.
    ///
    /// `local()` alone does not exercise this: its `from` is `"peer"`, which
    /// IS a well-formed Bus service name and passed the old check. The caller
    /// that failed is one with no registered name at all.
    #[test]
    fn an_unregistered_local_one_shot_can_drive_the_panel_verbs() {
        let frame = test_frame();
        let unregistered = |command: &str| {
            let mut request = local(command);
            request.from.clear();
            request
        };

        for command in [
            "shell.panel.pin",
            "shell.panel.show",
            "shell.panel.hide",
            "shell.panel.toggle",
            "shell.panel.unpin",
        ] {
            let (rc, body, command_out) =
                dispatch_shell_request(&unregistered(command), &frame, std::time::Duration::ZERO);
            assert_eq!(
                rc, 0,
                "{command} refused an unregistered local caller: {body}"
            );
            // rc alone is not acceptance. A regression that answered
            // `(0, accepted, None)` for anonymous callers specifically would
            // report success and do nothing, which is the shape a gate tends
            // to fail into.
            assert!(
                command_out.is_some(),
                "{command} returned rc=0 but enqueued no shell command"
            );
        }

        // The OTHER two relaxed sites. Each has its own `verify_caller_provenance`
        // call ahead of the shared one, so reverting either ALONE would restore
        // half the defect with every other test in both crates still green.
        let (rc, body, command_out) = dispatch_shell_request(
            &unregistered("shell.quit"),
            &frame,
            std::time::Duration::ZERO,
        );
        assert_eq!(
            rc, 0,
            "shell.quit refused an unregistered local caller: {body}"
        );
        assert_eq!(command_out.map(|c| c.kind), Some(ShellCommandKind::Quit));

        let mut resize = unregistered("shell.panel.resize");
        resize.body = r#"{"edge":"left","thickness_px":240}"#.into();
        let (rc, body, command_out) =
            dispatch_shell_request(&resize, &frame, std::time::Duration::ZERO);
        assert_eq!(
            rc, 0,
            "shell.panel.resize refused an unregistered local caller: {body}"
        );
        assert_eq!(
            command_out.map(|c| c.kind),
            Some(ShellCommandKind::ResizeCommit {
                edge: cosmix_shell::core::Edge::Left,
                thickness_px: 240.0
            })
        );

        // Correctness checks are NOT authorization and must still refuse: a
        // bad edge is a malformed operation whoever sends it.
        let mut bad_edge = unregistered("shell.panel.pin");
        bad_edge.body = r#"{"edge":"sideways"}"#.into();
        assert_eq!(
            dispatch_shell_request(&bad_edge, &frame, std::time::Duration::ZERO).0,
            10,
            "an invalid edge must still be refused"
        );

        // And provenance still fails closed for the same caller: a
        // self-asserted identity header is the broker's to stamp.
        let mut spoofed = unregistered("shell.panel.pin");
        spoofed
            .headers
            .insert("signed_ident".into(), "i-said-so".into());
        assert_eq!(
            dispatch_shell_request(&spoofed, &frame, std::time::Duration::ZERO).0,
            10,
            "a self-asserted signed_ident must still be refused"
        );
    }

    #[test]
    fn resize_requires_local_and_validates_range() {
        let frame = test_frame();
        // Unregistered caller is refused before any argument parsing.
        let mut anon = request("shell.panel.resize");
        anon.body = r#"{"edge":"left","thickness_px":240}"#.into();
        assert_eq!(
            dispatch_shell_request(&anon, &frame, std::time::Duration::ZERO).0,
            10
        );
        // A valid in-range resize returns a ResizeCommit for that edge.
        let mut ok = local("shell.panel.resize");
        ok.body = r#"{"edge":"left","thickness_px":240}"#.into();
        let (rc, body, command) = dispatch_shell_request(&ok, &frame, std::time::Duration::ZERO);
        assert_eq!(rc, 0);
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap(),
            json!({"accepted":true})
        );
        assert_eq!(
            command.unwrap().kind,
            ShellCommandKind::ResizeCommit {
                edge: cosmix_shell::core::Edge::Left,
                thickness_px: 240.0
            }
        );
        // Out-of-range and non-numeric thickness are refused with no command.
        for bad in [
            r#"{"edge":"left","thickness_px":40}"#,
            r#"{"edge":"left","thickness_px":9000}"#,
            r#"{"edge":"left","thickness_px":"wide"}"#,
            r#"{"edge":"nowhere","thickness_px":240}"#,
        ] {
            let mut req = local("shell.panel.resize");
            req.body = bad.into();
            let (rc, _, command) = dispatch_shell_request(&req, &frame, std::time::Duration::ZERO);
            assert_eq!(rc, 10, "rejected: {bad}");
            assert!(command.is_none(), "no command for: {bad}");
        }
    }

    #[test]
    fn resize_rejects_output_budget_and_accepts_exact_limit() {
        let mut frame = test_frame();
        frame.panels[Edge::Left.index()].max_thickness_px = 239.0;
        let mut req = local("shell.panel.resize");
        req.body = r#"{"edge":"left","thickness_px":240}"#.into();
        let (rc, body, command) = dispatch_shell_request(&req, &frame, std::time::Duration::ZERO);
        assert_eq!(rc, 10);
        assert!(command.is_none());
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap(),
            json!({
                "error_code":"PANEL_THICKNESS_BUDGET", "error":"panel thickness exceeds output budget", "edge":"left", "requested":240.0, "max":239.0
            })
        );
        req.body = r#"{"edge":"left","thickness_px":239}"#.into();
        let (rc, _, command) = dispatch_shell_request(&req, &frame, std::time::Duration::ZERO);
        assert_eq!(rc, 0);
        assert!(command.is_some());
    }

    /// A request shaped the way the LIVE wire delivers one: caller arguments
    /// in the JSON body (Mix's ordinary `send target cmd k=v` RPC), and the
    /// broker's own core-protocol headers stamped alongside — including the
    /// correlation `id`, which is what a header-first argument lookup used to
    /// mistake for the caller's `id=` argument.
    fn wire(command: &str, body: Value) -> InboundRequest {
        let mut request = local(command);
        request.body = body.to_string();
        for (name, value) in [
            ("bus", "1.0"),
            ("type", "request"),
            ("id", "7"),
            ("from", "peer"),
            ("to", "shell"),
            ("command", command),
        ] {
            request.headers.insert(name.to_owned(), value.to_owned());
        }
        request
    }

    /// Renamed 2026-09-21: the refusal below is a MISSING BROKER STAMP, not a
    /// missing registration. `request()` builds a frame with no
    /// `broker_origin` at all, so it fails provenance — which is what it
    /// always actually tested; the old name
    /// (`…_require_local_registration`) described a check that no longer
    /// exists on this path.
    #[test]
    fn read_surface_is_open_but_semantic_verbs_require_a_broker_stamped_origin() {
        let frame = test_frame();
        assert_eq!(
            dispatch_shell_request(&request("shell.ping"), &frame, Default::default()).0,
            0
        );
        assert_eq!(
            dispatch_shell_request(&request("shell.panel.show"), &frame, Default::default()).0,
            10
        );
        assert_eq!(
            dispatch_shell_request(&local("shell.panel.show"), &frame, Default::default()).0,
            0
        );
    }

    /// Renamed 2026-09-21 for the same reason as its neighbour: what this
    /// pins is that a corner verb behaves exactly like the panel verb it
    /// delegates to, under a broker-stamped caller. Registration stopped
    /// being part of the contract.
    #[test]
    fn corner_actions_share_panel_semantics_under_a_stamped_caller() {
        let frame = test_frame();
        for (corner, edge) in [
            ("top-left", "left"),
            ("bottom-left", "bottom"),
            ("bottom-right", "right"),
            ("top-right", "top"),
        ] {
            for action in ["show", "hide", "toggle", "pin", "unpin"] {
                let name = format!("shell.corner.{action}");
                let corner_request = wire(&name, json!({"corner":corner}));
                let panel_request = wire(&format!("shell.panel.{action}"), json!({"edge":edge}));
                let (rc, body, command) =
                    dispatch_shell_request(&corner_request, &frame, Default::default());
                assert_eq!(rc, 0, "{corner} {action}: {body}");
                assert_eq!(
                    command.unwrap().kind,
                    dispatch_shell_request(&panel_request, &frame, Default::default())
                        .2
                        .unwrap()
                        .kind
                );
                let mut unregistered = request(&name);
                unregistered.body = json!({"corner":corner}).to_string();
                let (rc, _, command) =
                    dispatch_shell_request(&unregistered, &frame, Default::default());
                assert_ne!(rc, 0);
                assert!(command.is_none());
            }
        }
        for body in [
            json!({}),
            json!({"corner":"invalid"}),
            json!({"corner":7}),
            json!({"edge":"left"}),
        ] {
            let (rc, _, command) = dispatch_shell_request(
                &wire("shell.corner.show", body),
                &frame,
                Default::default(),
            );
            assert_ne!(rc, 0);
            assert!(command.is_none());
        }
    }

    #[test]
    fn app_verbs_and_bad_page_arguments_get_precise_errors() {
        let frame = paged_frame();
        let (rc, body, command) =
            dispatch_shell_request(&request("app.ping"), &frame, Default::default());
        assert_eq!(rc, 10);
        assert!(body.contains("not supported by service shell"), "{body}");
        assert!(command.is_none());

        let (rc, body, _) =
            dispatch_shell_request(&local("shell.panel.page.set"), &frame, Default::default());
        assert_eq!(rc, 10);
        assert!(body.contains("requires an id"), "{body}");

        let set = wire(
            "shell.panel.page.set",
            json!({"edge":"left","id":"no-such"}),
        );
        let (rc, body, command) = dispatch_shell_request(&set, &frame, Default::default());
        assert_eq!(rc, 10);
        assert!(body.contains("unknown page id"), "{body}");
        assert!(
            command.is_none(),
            "an unacceptable verb must not be enqueued"
        );
    }

    /// The accepting path of `page.set`, over the wire shape a live citizen
    /// actually produces — the case no test covered, and the one the live
    /// gate caught in production.
    ///
    /// Every earlier `page.set` test ran against a frame whose carousels were
    /// empty, so *every* page id was unknown and the refusal branch was the
    /// only branch a test could ever reach. Under a realistic frame the
    /// broker's stamped correlation `id` header is what a header-first lookup
    /// read as the page id, so a valid `id=` in the body was rejected with
    /// "unknown page id for this edge" while `shell.props.get
    /// panels.<edge>.pages` advertised that same page — one frame, two
    /// argument sources. This pins the accepting branch AND that the stamped
    /// header does not shadow the caller's argument.
    #[test]
    fn a_valid_page_id_in_the_body_is_accepted_despite_the_brokers_stamped_id_header() {
        let frame = paged_frame();
        let set = wire(
            "shell.panel.page.set",
            json!({"edge":"bottom","id":"power"}),
        );
        assert_eq!(
            set.headers.get("id").map(String::as_str),
            Some("7"),
            "the wire fixture must carry a stamped correlation id — without it \
             this test cannot fail the way production did"
        );

        let (rc, body, command) = dispatch_shell_request(&set, &frame, Default::default());
        assert_eq!(rc, 0, "{body}");
        assert!(body.contains("\"accepted\":true"), "{body}");
        let command = command.expect("an accepted page.set must enqueue its command");
        assert_eq!(command.output, frame.geometry.output);
        assert_eq!(
            command.kind,
            ShellCommandKind::Carousel {
                edge: Edge::Bottom,
                input: CarouselInput::SelectId("power".to_owned()),
            },
            "the enqueued command must carry the CALLER's page id"
        );
    }

    /// The rest of the verb family reads `edge` — a name the transport does
    /// not own — so it was never shadowed; pinned here so a future change to
    /// argument resolution cannot break them silently while `page.set` keeps
    /// passing.
    #[test]
    fn every_panel_verb_resolves_its_edge_from_the_live_wire_shape() {
        let frame = paged_frame();
        for (command, body, expected) in [
            (
                "shell.panel.show",
                json!({"edge":"bottom"}),
                ShellCommandKind::Panel {
                    edge: Edge::Bottom,
                    input: PanelInput::Reveal,
                },
            ),
            (
                "shell.panel.hide",
                json!({"edge":"bottom"}),
                ShellCommandKind::Panel {
                    edge: Edge::Bottom,
                    input: PanelInput::Hide,
                },
            ),
            (
                "shell.panel.toggle",
                json!({"edge":"bottom"}),
                ShellCommandKind::Panel {
                    edge: Edge::Bottom,
                    input: PanelInput::Toggle,
                },
            ),
            (
                "shell.panel.pin",
                json!({"edge":"bottom"}),
                ShellCommandKind::Panel {
                    edge: Edge::Bottom,
                    input: PanelInput::Dock,
                },
            ),
            (
                "shell.panel.unpin",
                json!({"edge":"bottom"}),
                ShellCommandKind::Panel {
                    edge: Edge::Bottom,
                    input: PanelInput::Release,
                },
            ),
            (
                "shell.panel.dock",
                json!({"edge":"bottom"}),
                ShellCommandKind::Panel {
                    edge: Edge::Bottom,
                    input: PanelInput::Dock,
                },
            ),
            (
                "shell.panel.mode",
                json!({"edge":"bottom","mode":"pinned"}),
                ShellCommandKind::Panel {
                    edge: Edge::Bottom,
                    input: PanelInput::SetMode(PanelMode::Pinned),
                },
            ),
            (
                "shell.panel.page.next",
                json!({"edge":"bottom"}),
                ShellCommandKind::Carousel {
                    edge: Edge::Bottom,
                    input: CarouselInput::Next,
                },
            ),
            (
                "shell.panel.page.prev",
                json!({"edge":"bottom"}),
                ShellCommandKind::Carousel {
                    edge: Edge::Bottom,
                    input: CarouselInput::Previous,
                },
            ),
        ] {
            let request = wire(command, body);
            let (rc, body, enqueued) = dispatch_shell_request(&request, &frame, Default::default());
            assert_eq!(rc, 0, "{command}: {body}");
            assert_eq!(
                enqueued.expect("accepted verb enqueues a command").kind,
                expected,
                "{command}"
            );
        }
    }

    /// The precise dock verb: `Docked` is never a side effect of another
    /// verb (shell doc §3.1 — docking reflows the workspace), so it gets its
    /// own named verb enqueuing the model's `Dock` input for the addressed
    /// edge. Distinct from legacy `pin`, which happens to map to the same
    /// input only for popup-space compatibility.
    #[test]
    fn panel_dock_verb_enqueues_dock_input() {
        let frame = test_frame();
        let dock_request = wire("shell.panel.dock", json!({"edge":"right"}));
        let (rc, body, command) = dispatch_shell_request(&dock_request, &frame, Default::default());
        assert_eq!(rc, 0, "{body}");
        assert_eq!(
            command.expect("accepted dock verb enqueues a command").kind,
            ShellCommandKind::Panel {
                edge: Edge::Right,
                input: PanelInput::Dock,
            }
        );
        // An unstamped caller is refused like every other semantic verb.
        let (rc, _, command) = dispatch_shell_request(
            &request("shell.panel.dock"),
            &frame,
            Default::default(),
        );
        assert_eq!(rc, 10);
        assert!(command.is_none());
    }

    /// The precise mode verb carries its mode as a string argument: every
    /// token the `panels.<edge>.mode` leaf can emit is accepted, and anything
    /// else — missing, unknown, or the wrong JSON type — is refused with a
    /// precise error rather than the generic "unknown shell command".
    #[test]
    fn panel_mode_verb_validates_mode_string() {
        let frame = test_frame();
        for (token, mode) in [
            ("hidden", PanelMode::Hidden),
            ("pinned", PanelMode::Pinned),
            ("docked", PanelMode::Docked),
        ] {
            let request = wire("shell.panel.mode", json!({"edge":"left","mode":token}));
            let (rc, body, command) = dispatch_shell_request(&request, &frame, Default::default());
            assert_eq!(rc, 0, "{token}: {body}");
            assert_eq!(
                command
                    .expect("accepted mode verb enqueues a command")
                    .kind,
                ShellCommandKind::Panel {
                    edge: Edge::Left,
                    input: PanelInput::SetMode(mode),
                },
                "{token}"
            );
        }
        for (body, fragment) in [
            (json!({"edge":"left"}), "requires a mode argument"),
            (
                json!({"edge":"left","mode":"sideways"}),
                "mode must be hidden, pinned or docked",
            ),
            (
                json!({"edge":"left","mode":""}),
                "mode must be hidden, pinned or docked",
            ),
            (json!({"edge":"left","mode":7}), "requires a mode argument"),
        ] {
            let request = wire("shell.panel.mode", body.clone());
            let (rc, error, command) = dispatch_shell_request(&request, &frame, Default::default());
            assert_eq!(rc, 10, "{body}");
            assert!(error.contains(fragment), "{body}: {error}");
            assert!(command.is_none(), "{body} must not enqueue a command");
        }
    }

    /// The two surfaces the live gate found contradicting each other, read
    /// off ONE frame in one test: whatever `panels.<edge>.pages` advertises,
    /// `page.set` must accept.
    #[test]
    fn every_page_the_props_tree_advertises_is_accepted_by_page_set() {
        let frame = paged_frame();
        for edge in Edge::ALL {
            let name = edge_name(edge);
            for page in frame.panel(edge).page_ids.iter() {
                let request = wire(
                    "shell.panel.page.set",
                    json!({"edge": name, "id": page.clone()}),
                );
                let (rc, body, command) =
                    dispatch_shell_request(&request, &frame, Default::default());
                assert_eq!(rc, 0, "panels.{name}.pages advertises {page}: {body}");
                assert!(
                    command.is_some(),
                    "panels.{name}.pages advertises {page} but nothing was enqueued"
                );
            }
        }
    }

    /// An `App` carrying just what `service_bus` reads, driven by a bridge
    /// with no worker behind it.
    fn bus_app(bridge: BusBridge) -> App {
        let mut app = App::new();
        app.add_plugins(bevy::MinimalPlugins)
            .add_message::<ShellCommand>()
            .insert_resource(ShellFrameState(test_frame()))
            .insert_resource(bridge)
            .add_plugins(ShellBusPlugin);
        app
    }

    #[test]
    fn resize_reply_waits_for_model_and_rechecks_queued_geometry() {
        for shrink in [false, true] {
            let (bridge, peer) = test_bridge("quoin");
            let mut app = bus_app(bridge);
            let model = test_model();
            let output = model.output().clone();
            app.add_plugins(cosmix_shell::runtime::ShellRuntimePlugin::new(model));
            if shrink {
                app.world_mut().write_message(ShellCommand {
                    output,
                    at: Default::default(),
                    kind: ShellCommandKind::Geometry(
                        cosmix_shell::core::LogicalSize::new(200.0, 200.0).unwrap(),
                    ),
                });
            }
            let mut req = local("shell.panel.resize");
            req.body = r#"{"edge":"left","thickness_px":240}"#.into();
            peer.send(req);
            assert!(
                peer.drain_responses().is_empty(),
                "no speculative acceptance"
            );
            app.update();
            let replies = peer.drain_responses();
            assert_eq!(replies.len(), 1);
            let body: Value = serde_json::from_str(&replies[0].body).unwrap();
            if shrink {
                assert_eq!(replies[0].rc, 10);
                assert_eq!(body["error_code"], "PANEL_THICKNESS_BUDGET");
                assert_eq!(body["edge"], "left");
                assert_eq!(body["requested"], 240.0);
                assert!(body["max"].as_f64().unwrap() < 240.0);
            } else {
                assert_eq!(replies[0].rc, 0);
                assert_eq!(body["accepted"], true);
                assert_eq!(
                    app.world()
                        .resource::<ShellFrameState>()
                        .0
                        .panel(Edge::Left)
                        .thickness_px,
                    240.0
                );
            }
        }
    }

    #[test]
    fn missing_resize_receipts_expire_and_disconnect_clears_pending() {
        let (bridge, peer) = test_bridge("quoin");
        let mut app = bus_app(bridge);
        let mut req = local("shell.panel.resize");
        req.body = r#"{"edge":"left","thickness_px":240}"#.into();
        peer.send(req.clone());
        app.update();
        assert_eq!(
            app.world()
                .resource::<ShellBusState>()
                .pending_resizes
                .len(),
            1
        );
        for _ in 0..RESIZE_RECEIPT_FRAMES {
            app.update();
        }
        let replies = peer.drain_responses();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].rc, 10);
        assert_eq!(
            serde_json::from_str::<Value>(&replies[0].body).unwrap()["error_code"],
            "PANEL_RESIZE_TIMEOUT"
        );
        assert!(
            app.world()
                .resource::<ShellBusState>()
                .pending_resizes
                .is_empty()
        );
        peer.send(req);
        app.update();
        assert_eq!(
            app.world()
                .resource::<ShellBusState>()
                .pending_resizes
                .len(),
            1
        );
        peer.deliver_event(BusBridgeEvent::Fatal("test disconnect".into()));
        app.update();
        assert!(
            app.world()
                .resource::<ShellBusState>()
                .pending_resizes
                .is_empty()
        );
    }

    /// A `power.props.changed` delivery-gap notice on `generation`.
    fn gap_change(generation: u64) -> BusMessage {
        let mut headers = BTreeMap::new();
        headers.insert("topic".to_owned(), "power.props.changed".to_owned());
        headers.insert("gap".to_owned(), "true".to_owned());
        BusMessage {
            connection_generation: generation,
            from: "power".to_owned(),
            command: "noded.topic.event".to_owned(),
            body: "{}".to_owned(),
            headers,
        }
    }

    /// The `live_generation` gate itself — NOT `PowerSync`'s own sync
    /// generation (that one is `power.rs`'s
    /// `gap_recovery_is_keyed_on_the_sync_generation`).
    ///
    /// Honoring a stale-epoch `PowerAction::Resync` would land `Ready` on a
    /// dead generation and ignore live telemetry from then on — MAJOR 1's
    /// permanent "Power unavailable", restored. The gate's correctness
    /// otherwise rests entirely on an unwritten cross-crate invariant (the
    /// worker enqueues `Connected{g}` before any g-stamped message can be
    /// forwarded), so both directions are pinned here.
    #[test]
    fn the_live_generation_gate_refuses_a_stale_epoch_resync_and_honors_a_live_one() {
        let (bridge, peer) = test_bridge("quoin");
        let mut app = bus_app(bridge);

        // Connect on generation 2: the event records the live generation and
        // issues that epoch's own snapshot.
        peer.deliver_event(BusBridgeEvent::Connection {
            state: BusConnectionState::Connected,
            generation: 2,
        });
        app.update();
        let calls = peer.drain_calls();
        assert_eq!(
            calls
                .iter()
                .map(|call| call.command.as_str())
                .collect::<Vec<_>>(),
            [
                "power.props.get",
                "noded.props.get",
                "wallpaper.props.get",
                "background.status",
                "capture.status"
            ],
            "each projection bootstraps once"
        );
        let request_id = calls[0].request_id;

        // powerd is down: the snapshot fails, so the projection falls back to
        // Unavailable while the CONNECTION stays live on generation 2.
        peer.deliver_event(BusBridgeEvent::Reply {
            request_id,
            result: Err("powerd is down".to_owned()),
        });
        app.update();
        assert!(peer.drain_calls().is_empty());

        // A message forwarded under the OLD epoch, drained after the
        // reconnect. `PowerSync` holds no generation, so it asks for a resync
        // keyed on the message's own stale epoch; the gate must refuse it.
        peer.deliver_message(gap_change(1));
        app.update();
        assert!(
            peer.drain_calls().is_empty(),
            "a stale-epoch resync must not issue a request"
        );

        // The same notice on the live epoch must start a sync — refusing
        // everything would be the other half of MAJOR 1.
        peer.deliver_message(gap_change(2));
        app.update();
        let calls = peer.drain_calls();
        assert_eq!(
            calls.len(),
            2,
            "a live-generation gap resyncs both projections"
        );
        assert_eq!(calls[0].command, "noded.props.get");
        assert_eq!(calls[1].command, "power.props.get");
    }

    /// The drain order the gate depends on: `drain_events` BEFORE
    /// `drain_messages`, so a connect and a message forwarded under the same
    /// generation both take effect in one frame.
    ///
    /// Reversed, `live_generation` would still be unset when the message is
    /// read, the gate would refuse it, and only the Connected event's own
    /// snapshot would appear — one call, not two.
    #[test]
    fn events_drain_before_messages_within_one_frame() {
        let (bridge, peer) = test_bridge("quoin");
        let mut app = bus_app(bridge);
        peer.deliver_event(BusBridgeEvent::Connection {
            state: BusConnectionState::Connected,
            generation: 2,
        });
        peer.deliver_message(gap_change(2));
        app.update();
        assert_eq!(
            peer.drain_calls()
                .iter()
                .map(|call| call.command.as_str())
                .collect::<Vec<_>>(),
            [
                "power.props.get",
                "noded.props.get",
                "noded.props.get",
                "power.props.get",
                "wallpaper.props.get",
                "background.status",
                "capture.status"
            ],
            "drain_events must run before drain_messages"
        );
    }

    /// A broker registry observation. Consumers must reconcile against `new`,
    /// even if a dropped or coalesced event omitted the owner from `old`.
    fn services_registered_change(generation: u64, old: &[&str], new: &[&str]) -> BusMessage {
        let mut headers = BTreeMap::new();
        headers.insert("topic".to_owned(), "noded.props.changed".to_owned());
        BusMessage {
            connection_generation: generation,
            from: "noded".to_owned(),
            command: "noded.topic.event".to_owned(),
            body: json!({
                "path": "services.registered",
                "old": old,
                "new": new,
                "cause": "disconnect:quoin-panel",
            })
            .to_string(),
            headers,
        }
    }

    fn mounted_bus_app() -> (App, ctk::bus::TestBusPeer) {
        use cosmix_shell::chrome::{
            QuoinChromePlugin, QuoinContentBindings, QuoinPageRegistry, QuoinPanelMounts,
            spawn_quoin_chrome,
        };
        let (bridge, peer) = test_bridge("quoin");
        let mut app = bus_app(bridge);
        app.add_plugins(cosmix_shell::runtime::ShellRuntimePlugin::new(test_model()))
            .add_plugins(QuoinChromePlugin)
            .init_resource::<ButtonInput<KeyCode>>()
            .add_systems(
                Update,
                cosmix_scene_bevy::reconcile_scene_mounts
                    .after(ShellRuntimeSet::Input)
                    .before(ShellRuntimeSet::Model),
            );
        let world = app.world_mut();
        let props = QuoinPageRegistry::new(vec![], vec![], vec![], vec![])
            .unwrap()
            .bind(
                &world.resource::<ShellFrameState>().0,
                QuoinContentBindings::default(),
            )
            .unwrap();
        let mounts = QuoinPanelMounts::new(
            world.spawn_empty().id(),
            world.spawn_empty().id(),
            world.spawn_empty().id(),
            world.spawn_empty().id(),
        );
        spawn_quoin_chrome(&mut world.commands(), mounts, props);
        world.flush();
        peer.deliver_event(BusBridgeEvent::Connection {
            state: BusConnectionState::Connected,
            generation: 1,
        });
        app.update();
        peer.drain_calls();
        (app, peer)
    }

    fn scene_load(name: &str, owner: &str, edge: &str) -> InboundRequest {
        let mut req = local("shell.scene.load");
        req.from = owner.into();
        // Deliberately unrelated metadata: it must never choose lifetime ownership.
        req.body = format!(
            "---\nscene: 1\nname: {name}\ncitizen: authored-metadata\nwindow: {{\"kind\":\"edge\",\"edge\":\"{edge}\"}}\n---\n```mix\nroot: {{widget: \"column\", children: []}}\n```\n"
        );
        req
    }

    fn load_scene(
        app: &mut App,
        peer: &ctk::bus::TestBusPeer,
        name: &str,
        owner: &str,
        edge: &str,
    ) {
        peer.send(scene_load(name, owner, edge));
        app.update();
        let replies = peer.drain_responses();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].rc, 0, "{}", replies[0].body);
        assert!(
            app.world()
                .resource::<ShellFrameState>()
                .0
                .panel(match edge {
                    "left" => Edge::Left,
                    "right" => Edge::Right,
                    "top" => Edge::Top,
                    _ => Edge::Bottom,
                })
                .page_ids
                .iter()
                .any(|id| id == &format!("scene-{name}")),
            "fixture must have real mounted chrome"
        );
    }

    fn absent(peer: &ctk::bus::TestBusPeer) {
        // The missed owner's name is not even in old: old-minus-new cannot pass.
        peer.deliver_message(services_registered_change(
            1,
            &["shell"],
            &["shell", "keeper"],
        ));
    }

    #[test]
    fn citizen_disconnect_removes_owned_subpanels_and_scenes() {
        let (mut app, peer) = mounted_bus_app();
        load_scene(&mut app, &peer, "alpha", "keeper", "left");
        load_scene(&mut app, &peer, "beta", "keeper", "left");
        load_scene(&mut app, &peer, "notes", "owner", "left");
        load_scene(&mut app, &peer, "other-edge", "owner", "right");
        // Mounting only registers; select the owner's page so disconnect must
        // exercise removal landing and selection-memory fallback.
        app.world_mut().write_message(ShellCommand {
            output: test_model().output().clone(),
            at: Default::default(),
            kind: ShellCommandKind::Carousel {
                edge: Edge::Left,
                input: CarouselInput::SelectId("scene-notes".into()),
            },
        });
        app.update();
        assert_eq!(
            app.world()
                .resource::<ShellFrameState>()
                .0
                .panel(Edge::Left)
                .active_page_id
                .as_deref(),
            Some("scene-notes")
        );
        peer.deliver_message(services_registered_change(0, &["owner"], &[]));
        app.update();
        assert!(
            app.world()
                .resource::<SubPanelRegistryState>()
                .0
                .seat("scene-notes")
                .is_some()
        );
        absent(&peer);
        app.update();
        assert!(
            app.world()
                .resource::<SubPanelRegistryState>()
                .0
                .names_owned_by("owner")
                .is_empty()
        );
        assert!(
            app.world()
                .resource::<cosmix_scene_bevy::SceneStore>()
                .scenes_owned_by("owner")
                .is_empty()
        );
        assert_eq!(
            app.world()
                .resource::<ShellFrameState>()
                .0
                .panel(Edge::Left)
                .active_page_id
                .as_deref(),
            Some("scene-beta")
        );
        assert!(
            app.world()
                .resource::<ShellFrameState>()
                .0
                .panel(Edge::Right)
                .page_ids
                .is_empty()
        );
        // Actual scene teardown has run. A fresh reveal must remember the primary,
        // not rewrite the previous-neighbour landing as last_selected.
        for input in [PanelInput::Hide, PanelInput::Reveal] {
            app.world_mut().write_message(ShellCommand {
                output: test_model().output().clone(),
                at: Default::default(),
                kind: ShellCommandKind::Panel {
                    edge: Edge::Left,
                    input,
                },
            });
            app.update();
        }
        assert_eq!(
            app.world()
                .resource::<ShellFrameState>()
                .0
                .panel(Edge::Left)
                .active_page_id
                .as_deref(),
            Some("scene-alpha")
        );
    }

    #[test]
    fn citizen_disconnect_queued_before_replacement_load_preserves_replacement() {
        let (mut app, peer) = mounted_bus_app();
        load_scene(&mut app, &peer, "notes", "owner", "left");
        load_scene(&mut app, &peer, "old-only", "owner", "right");
        absent(&peer);
        peer.send(scene_load("notes", "owner", "left"));
        app.update();
        assert_eq!(peer.drain_responses()[0].rc, 0);
        let registry = &app.world().resource::<SubPanelRegistryState>().0;
        assert!(registry.seat("scene-notes").is_some());
        assert!(registry.seat("scene-old-only").is_none());
        assert_eq!(
            app.world()
                .resource::<cosmix_scene_bevy::SceneStore>()
                .scenes_owned_by("owner"),
            ["notes"]
        );
        assert_eq!(
            app.world()
                .resource::<ShellFrameState>()
                .0
                .panel(Edge::Left)
                .active_page_id
                .as_deref(),
            Some("scene-notes")
        );
    }

    #[test]
    fn citizen_scene_load_rejects_cross_owner_output_and_edge_collisions() {
        let (mut app, peer) = mounted_bus_app();
        load_scene(&mut app, &peer, "notes", "owner", "left");
        for (owner, edge) in [("other", "left"), ("owner", "right")] {
            peer.send(scene_load("notes", owner, edge));
            app.update();
            let replies = peer.drain_responses();
            assert_eq!(replies[0].rc, 10);
            assert!(replies[0].body.contains("SUBPANEL_COLLISION"));
            assert_eq!(
                app.world()
                    .resource::<SubPanelRegistryState>()
                    .0
                    .seat("scene-notes")
                    .unwrap()
                    .owner,
                "owner"
            );
        }
        // Reserve elsewhere, then enter through the actual scene-load mount path.
        app.world_mut()
            .resource_mut::<SubPanelRegistryState>()
            .0
            .mount(
                "scene-remote",
                cosmix_shell::core::OutputKey::new("other-output").unwrap(),
                Edge::Left,
                "owner",
                0,
            )
            .unwrap();
        peer.send(scene_load("remote", "owner", "left"));
        app.update();
        assert_eq!(peer.drain_responses()[0].rc, 10);
        assert_eq!(
            app.world()
                .resource::<cosmix_scene_bevy::SceneStore>()
                .scenes_owned_by("owner"),
            ["notes"]
        );
        // Same owner/seat is a successful update, regardless of authored citizen.
        load_scene(&mut app, &peer, "notes", "owner", "left");
        assert!(
            app.world()
                .resource::<cosmix_scene_bevy::SceneStore>()
                .scenes_owned_by("authored-metadata")
                .is_empty()
        );
    }

    fn citizen_snapshot_id(peer: &ctk::bus::TestBusPeer) -> u64 {
        peer.drain_calls()
            .into_iter()
            .find(|call| call.command == "noded.props.get")
            .expect("registry reconciliation must request a full snapshot")
            .request_id
    }

    fn reply_citizen_snapshot(peer: &ctk::bus::TestBusPeer, request_id: u64, names: &[&str]) {
        peer.deliver_event(BusBridgeEvent::Reply {
            request_id,
            result: Ok(ctk::bus::BusReply {
                rc: 0,
                result: None,
                body: json!({"services":{"registered":names}}).to_string(),
            }),
        });
    }

    #[test]
    fn citizen_reconnect_and_dropped_messages_reconcile_full_snapshot() {
        for trigger in [
            BusBridgeEvent::Connection {
                state: BusConnectionState::Connected,
                generation: 2,
            },
            BusBridgeEvent::DroppedMessages(1),
            BusBridgeEvent::ObservationDroppedMessages(1),
        ] {
            let (mut app, peer) = mounted_bus_app();
            load_scene(&mut app, &peer, "notes", "owner", "left");
            peer.deliver_event(trigger);
            app.update();
            let id = citizen_snapshot_id(&peer);
            reply_citizen_snapshot(&peer, id, &["shell"]);
            app.update();
            assert!(
                app.world()
                    .resource::<SubPanelRegistryState>()
                    .0
                    .seat("scene-notes")
                    .is_none()
            );
            assert!(
                app.world()
                    .resource::<ShellFrameState>()
                    .0
                    .panel(Edge::Left)
                    .page_ids
                    .is_empty()
            );
            app.update();
            assert!(peer.drain_calls().is_empty(), "no periodic polling");
        }
    }

    #[test]
    fn citizen_snapshot_cannot_remove_load_accepted_after_request() {
        let (mut app, peer) = mounted_bus_app();
        load_scene(&mut app, &peer, "notes", "owner", "left");
        peer.deliver_event(BusBridgeEvent::DroppedMessages(1));
        app.update();
        let id = citizen_snapshot_id(&peer);
        load_scene(&mut app, &peer, "notes", "owner", "left");
        reply_citizen_snapshot(&peer, id, &[]);
        app.update();
        assert!(
            app.world()
                .resource::<SubPanelRegistryState>()
                .0
                .seat("scene-notes")
                .is_some()
        );
        // A later observation still cleans this seat when it really disappears.
        absent(&peer);
        app.update();
        assert!(
            app.world()
                .resource::<SubPanelRegistryState>()
                .0
                .seat("scene-notes")
                .is_none()
        );
    }

    #[test]
    fn citizen_malformed_snapshot_and_stale_reply_remove_nothing() {
        let (mut app, peer) = mounted_bus_app();
        load_scene(&mut app, &peer, "notes", "owner", "left");
        peer.deliver_event(BusBridgeEvent::DroppedMessages(1));
        app.update();
        let id = citizen_snapshot_id(&peer);
        peer.deliver_message(services_registered_change(1, &[], &["owner"]));
        app.update();
        reply_citizen_snapshot(&peer, id, &[]);
        let mut malformed = services_registered_change(1, &[], &[]);
        malformed.body = json!({"path":"services.registered", "new":["shell", 3]}).to_string();
        peer.deliver_message(malformed);
        app.update();
        assert!(
            app.world()
                .resource::<SubPanelRegistryState>()
                .0
                .seat("scene-notes")
                .is_some()
        );
    }

    #[test]
    fn citizen_mesh_and_anonymous_scene_callers_remain_open() {
        let (mut app, peer) = mounted_bus_app();
        let mut remote = scene_load("remote", "bridge-peer", "left");
        remote.headers.insert("broker_origin".into(), "mesh".into());
        remote
            .headers
            .insert("broker_peer".into(), "remote-node".into());
        remote
            .headers
            .insert("broker_service".into(), "notes".into());
        peer.send(remote);
        peer.send(scene_load("anonymous", "", "right"));
        app.update();
        let replies = peer.drain_responses();
        assert_eq!(replies.len(), 2);
        assert!(replies.iter().all(|reply| reply.rc == 0));
        absent(&peer);
        app.update();
        let registry = &app.world().resource::<SubPanelRegistryState>().0;
        assert_eq!(
            registry.seat("scene-remote").unwrap().owner,
            "notes@remote-node"
        );
        assert!(registry.seat("scene-anonymous").is_some());
        // No local registration set can attest either lifetime. Do not guess.
        // Nor may another anonymous request silently become that same owner.
        peer.send(scene_load("anonymous", "", "right"));
        app.update();
        assert_eq!(peer.drain_responses()[0].rc, 10);
    }

    #[test]
    fn citizen_scene_load_refuses_stale_connection_and_unattested_sender() {
        let (mut app, peer) = mounted_bus_app();
        let mut stale = scene_load("stale", "owner", "left");
        stale.connection_generation = 0;
        peer.send(stale);
        app.update();
        // The bridge drops stale epochs before Quoin dispatch. Their reply
        // correlation belongs to a dead connection, so no reply is expected.
        assert!(peer.drain_responses().is_empty());
        assert!(
            app.world()
                .resource::<SubPanelRegistryState>()
                .0
                .names_owned_by("owner")
                .is_empty()
        );
        assert!(
            app.world()
                .resource::<cosmix_scene_bevy::SceneStore>()
                .scenes_owned_by("owner")
                .is_empty()
        );
        let mut unattested = scene_load("unattested", "owner", "left");
        unattested.headers.clear();
        peer.send(unattested);
        app.update();
        let replies = peer.drain_responses();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].rc, 10);
        assert!(replies[0].body.contains("scene caller provenance"));
        assert!(
            app.world()
                .resource::<SubPanelRegistryState>()
                .0
                .names_owned_by("owner")
                .is_empty()
        );
        assert!(
            app.world()
                .resource::<cosmix_scene_bevy::SceneStore>()
                .scenes_owned_by("owner")
                .is_empty()
        );
        assert!(
            app.world()
                .resource::<ShellFrameState>()
                .0
                .panel(Edge::Left)
                .page_ids
                .is_empty()
        );
        // Positive control: the same live, attested sender can still load.
        load_scene(&mut app, &peer, "accepted", "owner", "left");
    }

    /// The settings branch keeps the same stale-connection fence as the scene
    /// and sub-panel verbs. The test bridge's committed generation stays 1,
    /// so a request stamped 1 still drains after a `Connected` event for
    /// generation 2 — exactly the leaked-epoch shape the fence exists for:
    /// the request must be refused before `dispatch_verb`, spending no
    /// settings write (here, no live theme application).
    #[test]
    fn settings_verbs_refuse_a_stale_quoin_connection() {
        let (bridge, peer) = test_bridge("quoin");
        let mut app = bus_app(bridge);
        // Before any connection, the fence is open (no live generation to
        // mismatch) and the write spends normally.
        peer.send(wire("shell.settings.scheme", json!({"name":"forest"})));
        app.update();
        let replies = peer.drain_responses();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].rc, 0, "{}", replies[0].body);
        assert_eq!(
            app.world_mut()
                .resource_mut::<Messages<cosmix_shell::chrome::QuoinSchemeSelected>>()
                .drain()
                .count(),
            1,
            "the live request applied the theme"
        );
        peer.deliver_event(BusBridgeEvent::Connection {
            state: BusConnectionState::Connected,
            generation: 2,
        });
        app.update();
        peer.drain_calls();
        peer.send(wire("shell.settings.scheme", json!({"name":"forest"})));
        app.update();
        let replies = peer.drain_responses();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].rc, 10);
        assert!(
            replies[0].body.contains("stale Quoin connection"),
            "{}",
            replies[0].body
        );
        assert_eq!(
            app.world_mut()
                .resource_mut::<Messages<cosmix_shell::chrome::QuoinSchemeSelected>>()
                .drain()
                .count(),
            0,
            "a stale settings request must not apply a theme"
        );
    }

    /// Chunk-8 fixture: the sub-panel verbs address the process-wide
    /// registry and the model's carousels, which the runtime plugin owns —
    /// no chrome needed, so a plain bus app with its model connected.
    fn sub_panel_app() -> (App, ctk::bus::TestBusPeer) {
        let (bridge, peer) = test_bridge("quoin");
        let mut app = bus_app(bridge);
        app.add_plugins(cosmix_shell::runtime::ShellRuntimePlugin::new(test_model()));
        peer.deliver_event(BusBridgeEvent::Connection {
            state: BusConnectionState::Connected,
            generation: 1,
        });
        app.update();
        peer.drain_calls();
        (app, peer)
    }

    /// Send one sub-panel verb over the live wire shape and drain its reply.
    fn sub_send(
        app: &mut App,
        peer: &ctk::bus::TestBusPeer,
        command: &str,
        body: Value,
    ) -> (u8, Value) {
        peer.send(wire(command, body));
        app.update();
        let replies = peer.drain_responses();
        assert_eq!(
            replies.len(),
            1,
            "one request must produce exactly one reply"
        );
        (
            replies[0].rc,
            serde_json::from_str(&replies[0].body).unwrap(),
        )
    }

    #[test]
    fn sub_register_remove_round_trip() {
        let (mut app, peer) = sub_panel_app();
        // Ownership is the attested caller; a caller-supplied `owner`
        // argument is never read.
        let (rc, body) = sub_send(
            &mut app,
            &peer,
            "shell.sub.register",
            json!({"edge":"left","name":"notify.n42","owner":"spoofed"}),
        );
        assert_eq!(rc, 0, "{body}");
        assert_eq!(body["accepted"], true);
        assert_eq!(
            app.world()
                .resource::<ShellFrameState>()
                .0
                .panel(Edge::Left)
                .page_ids
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["notify.n42"],
            "registration fills the carousel without chrome content"
        );
        let registry = &app.world().resource::<SubPanelRegistryState>().0;
        let seat = registry.seat("notify.n42").unwrap();
        assert_eq!(seat.owner, "peer", "the attested caller owns the seat");
        assert_eq!(seat.edge, Edge::Left);
        assert!(
            seat.accepted_at >= 1,
            "the seat carries its acceptance receipt for disconnect sweeps"
        );

        let (rc, body) = sub_send(
            &mut app,
            &peer,
            "shell.sub.remove",
            json!({"name":"notify.n42"}),
        );
        assert_eq!(rc, 0, "{body}");
        assert!(
            app.world()
                .resource::<ShellFrameState>()
                .0
                .panel(Edge::Left)
                .page_ids
                .is_empty()
        );
        assert!(
            app.world()
                .resource::<SubPanelRegistryState>()
                .0
                .seat("notify.n42")
                .is_none()
        );

        // The name is free again after removal — for anyone.
        let (rc, body) = sub_send(
            &mut app,
            &peer,
            "shell.sub.register",
            json!({"edge":"right","name":"notify.n42"}),
        );
        assert_eq!(rc, 0, "{body}");
        assert!(
            app.world()
                .resource::<SubPanelRegistryState>()
                .0
                .seat("notify.n42")
                .is_some()
        );
    }

    #[test]
    fn sub_register_duplicate_name_refused() {
        let (mut app, peer) = sub_panel_app();
        let (rc, _) = sub_send(
            &mut app,
            &peer,
            "shell.sub.register",
            json!({"edge":"left","name":"notify.n42"}),
        );
        assert_eq!(rc, 0);
        // The same name again — same edge and owner, then another edge: a
        // name is globally unique across all edges and outputs (panel doc
        // §5), and both refusals happen at dispatch, before any ack.
        for edge in ["left", "right"] {
            let (rc, body) = sub_send(
                &mut app,
                &peer,
                "shell.sub.register",
                json!({"edge":edge,"name":"notify.n42"}),
            );
            assert_eq!(rc, 10, "{edge}: {body}");
            assert!(
                body["error"]
                    .as_str()
                    .unwrap()
                    .contains("already registered"),
                "{edge}: {body}"
            );
        }
        // Neither refusal disturbed the original registration.
        let frame = &app.world().resource::<ShellFrameState>().0;
        assert_eq!(
            frame
                .panel(Edge::Left)
                .page_ids
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["notify.n42"]
        );
        assert!(frame.panel(Edge::Right).page_ids.is_empty());
        assert_eq!(
            app.world()
                .resource::<SubPanelRegistryState>()
                .0
                .seat("notify.n42")
                .unwrap()
                .owner,
            "peer"
        );
        // A live page WITHOUT a seat (host chrome content) is equally taken,
        // wherever it sits: the frame check sweeps every edge of the output,
        // so a seat-less page on ANOTHER edge refuses a registration that
        // the requested edge alone would have accepted.
        cosmix_shell::runtime::set_shell_pages(
            app.world_mut(),
            Edge::Bottom,
            vec!["launcher".to_owned()],
            None,
        );
        for edge in ["bottom", "right"] {
            let (rc, body) = sub_send(
                &mut app,
                &peer,
                "shell.sub.register",
                json!({"edge":edge,"name":"launcher"}),
            );
            assert_eq!(rc, 10, "{edge}: {body}");
            assert!(
                body["error"].as_str().unwrap().contains("already a page"),
                "{edge}: {body}"
            );
        }
    }

    #[test]
    fn sub_remove_unknown_is_refused() {
        let (mut app, peer) = sub_panel_app();
        let (rc, body) = sub_send(&mut app, &peer, "shell.sub.remove", json!({"name":"ghost"}));
        assert_eq!(rc, 10, "{body}");
        assert!(
            body["error"].as_str().unwrap().contains("not registered"),
            "{body}"
        );
        // Malformed requests are refused with precise errors; nothing
        // enqueues and no seat is spent.
        for (command, body_value, fragment) in [
            ("shell.sub.remove", json!({}), "requires a name"),
            ("shell.sub.remove", json!({"name":"  "}), "requires a name"),
            ("shell.sub.register", json!({"name":"x"}), "edge must be"),
            (
                "shell.sub.register",
                json!({"edge":"sideways","name":"x"}),
                "edge must be",
            ),
        ] {
            let (rc, body) = sub_send(&mut app, &peer, command, body_value.clone());
            assert_eq!(rc, 10, "{command} {body_value}: {body}");
            assert!(body["error"].as_str().unwrap().contains(fragment));
        }
        // An unattested caller is refused before any argument is read — the
        // provenance stamp every verb requires, not a "who may" gate.
        let mut unattested = request("shell.sub.register");
        unattested.body = json!({"edge":"left","name":"x"}).to_string();
        peer.send(unattested);
        app.update();
        let replies = peer.drain_responses();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].rc, 10);
        assert!(replies[0].body.contains("sub-panel caller provenance"));
        assert!(
            app.world()
                .resource::<SubPanelRegistryState>()
                .0
                .seat("x")
                .is_none()
        );
    }

    #[test]
    fn sub_remove_lands_per_removal_rule() {
        let (mut app, peer) = sub_panel_app();
        for name in ["alpha", "beta", "gamma"] {
            let (rc, body) = sub_send(
                &mut app,
                &peer,
                "shell.sub.register",
                json!({"edge":"left","name":name}),
            );
            assert_eq!(rc, 0, "{name}: {body}");
        }
        let pages = |app: &App| {
            app.world()
                .resource::<ShellFrameState>()
                .0
                .panel(Edge::Left)
                .page_ids
                .iter()
                .cloned()
                .collect::<Vec<String>>()
        };
        let active = |app: &App| {
            app.world()
                .resource::<ShellFrameState>()
                .0
                .panel(Edge::Left)
                .active_page_id
                .clone()
        };
        assert_eq!(pages(&app), ["alpha", "beta", "gamma"]);
        // Activation is chunk 16; page.set selects today.
        let (rc, body) = sub_send(
            &mut app,
            &peer,
            "shell.panel.page.set",
            json!({"edge":"left","id":"gamma"}),
        );
        assert_eq!(rc, 0, "{body}");
        assert_eq!(active(&app).as_deref(), Some("gamma"));

        // Removing the shown page lands on the previous registered
        // neighbour (panel doc §3).
        let (rc, body) = sub_send(&mut app, &peer, "shell.sub.remove", json!({"name":"gamma"}));
        assert_eq!(rc, 0, "{body}");
        assert_eq!(active(&app).as_deref(), Some("beta"));
        assert_eq!(pages(&app), ["alpha", "beta"]);

        // The remembered selection fell back to the primary, not to the
        // landing: a fresh reveal shows alpha (chunk 5's rule, reached
        // through the verb).
        for verb in ["shell.panel.hide", "shell.panel.show"] {
            let (rc, body) = sub_send(&mut app, &peer, verb, json!({"edge":"left"}));
            assert_eq!(rc, 0, "{verb}: {body}");
        }
        assert_eq!(active(&app).as_deref(), Some("alpha"));

        // Removing a page that is not shown never moves the selection.
        let (rc, body) = sub_send(&mut app, &peer, "shell.sub.remove", json!({"name":"beta"}));
        assert_eq!(rc, 0, "{body}");
        assert_eq!(active(&app).as_deref(), Some("alpha"));
        assert_eq!(pages(&app), ["alpha"]);
        assert_eq!(
            app.world()
                .resource::<SubPanelRegistryState>()
                .0
                .names_owned_by("peer"),
            ["alpha"],
            "each removal took its seat with its page"
        );
    }

    /// Review #4 (chunk 8): a queued removal applies only to the exact
    /// registration its dispatch resolved — same owner AND same acceptance
    /// receipt — never to whatever holds the name at the Model stage. In
    /// one batch: `sub.remove` queues against the seat; a scene unload
    /// drops that seat; another caller's load reserves a replacement under
    /// the same name. The stale removal must drop silently and the
    /// replacement survive.
    #[test]
    fn sub_remove_spared_a_replacement_reserved_in_the_same_batch() {
        let (mut app, peer) = mounted_bus_app();
        load_scene(&mut app, &peer, "notes", "owner", "left");
        // Arrival order is the race: the removal resolves the live seat
        // first, the unload then drops it, and the replacement load
        // re-reserves the name before the Model stage drains the queue.
        peer.send(wire("shell.sub.remove", json!({"name":"scene-notes"})));
        peer.send(wire("shell.scene.unload", json!({"scene":"notes"})));
        peer.send(scene_load("notes", "other", "left"));
        app.update();
        let replies = peer.drain_responses();
        assert_eq!(replies.len(), 3);
        for reply in &replies {
            assert_eq!(reply.rc, 0, "{}", reply.body);
        }
        let registry = &app.world().resource::<SubPanelRegistryState>().0;
        let seat = registry.seat("scene-notes").expect("replacement seat");
        assert_eq!(seat.owner, "other");
        assert_eq!(
            app.world()
                .resource::<cosmix_scene_bevy::SceneStore>()
                .scenes_owned_by("other"),
            ["notes"]
        );
        assert!(
            app.world()
                .resource::<ShellFrameState>()
                .0
                .panel(Edge::Left)
                .page_ids
                .iter()
                .any(|page| page == "scene-notes"),
            "the replacement's carousel page survives the stale removal"
        );
        // The spared replacement is itself removable through a fresh verb.
        let (rc, body) = sub_send(
            &mut app,
            &peer,
            "shell.sub.remove",
            json!({"name":"scene-notes"}),
        );
        assert_eq!(rc, 0, "{body}");
        assert!(
            app.world()
                .resource::<SubPanelRegistryState>()
                .0
                .seat("scene-notes")
                .is_none()
        );
    }

    /// Review #5 (chunk 8): a dispatch reserves its seat and queues its
    /// command against the current output; an output replacement landing
    /// before the Model stage (the embedded host's model swap — now ordered
    /// before the dispatch — or a stashed reply drained a frame later)
    /// migrates the seat. The Model stage must apply the lifecycle command
    /// against the replacement model rather than drop it at the output
    /// gate: the register fills the replacement's carousel, and a later
    /// acked removal still lands.
    #[test]
    fn lifecycle_commands_apply_across_an_output_replacement() {
        #[derive(Resource)]
        struct PendingReplacement(cosmix_shell::core::ShellModel);
        // A one-shot host system standing in for the embedded `prepare`
        // race: it replaces the model after the Input stage has dispatched
        // against the outgoing output, and before the Model stage applies.
        fn replace_output(world: &mut World) {
            if let Some(pending) = world.remove_resource::<PendingReplacement>() {
                cosmix_shell::runtime::replace_shell_model(world, pending.0);
            }
        }
        fn model_on(output: &str) -> cosmix_shell::core::ShellModel {
            cosmix_shell::core::ShellModel::new(
                cosmix_shell::core::OutputKey::new(output).unwrap(),
                cosmix_shell::core::LogicalSize::new(1000.0, 800.0).unwrap(),
                Default::default(),
                std::time::Duration::from_millis(800),
                std::time::Duration::from_millis(200),
            )
            .unwrap()
        }

        let (bridge, peer) = test_bridge("quoin");
        let mut app = bus_app(bridge);
        app.add_plugins(cosmix_shell::runtime::ShellRuntimePlugin::new(model_on("test")))
            .add_systems(
                Update,
                replace_output
                    .after(ShellRuntimeSet::Input)
                    .before(ShellRuntimeSet::Model),
            );
        peer.deliver_event(BusBridgeEvent::Connection {
            state: BusConnectionState::Connected,
            generation: 1,
        });
        app.update();
        peer.drain_calls();

        // Register dispatched against "test"; the model becomes
        // "replacement" between dispatch and application.
        peer.send(wire(
            "shell.sub.register",
            json!({"edge":"left","name":"notify.migrate"}),
        ));
        app.insert_resource(PendingReplacement(model_on("replacement")));
        app.update();
        assert_eq!(peer.drain_responses()[0].rc, 0);
        let frame = &app.world().resource::<ShellFrameState>().0;
        assert_eq!(frame.geometry.output.as_str(), "replacement");
        assert_eq!(
            frame
                .panel(Edge::Left)
                .page_ids
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["notify.migrate"],
            "a register dispatched against the replaced output fills the \
             replacement's carousel"
        );
        assert_eq!(
            app.world()
                .resource::<SubPanelRegistryState>()
                .0
                .seat("notify.migrate")
                .unwrap()
                .output
                .as_str(),
            "replacement"
        );

        // An acked removal across a second replacement still lands: the
        // seat migrates again and the Model stage applies the removal
        // against the current registry.
        peer.send(wire("shell.sub.remove", json!({"name":"notify.migrate"})));
        app.insert_resource(PendingReplacement(model_on("third")));
        app.update();
        assert_eq!(peer.drain_responses()[0].rc, 0);
        let frame = &app.world().resource::<ShellFrameState>().0;
        assert_eq!(frame.geometry.output.as_str(), "third");
        assert!(
            frame.panel(Edge::Left).page_ids.is_empty(),
            "the removal took the migrated page with its seat"
        );
        assert!(
            app.world()
                .resource::<SubPanelRegistryState>()
                .0
                .seat("notify.migrate")
                .is_none()
        );
    }

    /// A reply that failed because the worker is GONE must be dropped, not
    /// stashed: nothing will ever drain the queue, and no `Fatal` event can
    /// arrive to clear the stash because the worker owned that channel too.
    #[test]
    fn a_dead_worker_drops_the_reply_instead_of_retrying_it_forever() {
        let (bridge, peer) = test_bridge("quoin");
        peer.send(request("shell.ping"));
        // The worker dies with the request still queued.
        drop(peer);
        let mut app = bus_app(bridge);
        app.update();
        assert!(
            app.world()
                .resource::<ShellBusState>()
                .pending_replies
                .is_empty(),
            "a reply that can never be sent must be dropped, not retried every frame"
        );
    }

    /// The other half of that distinction: a merely FULL channel is
    /// retryable, so the reply is still stashed.
    #[test]
    fn a_full_outbound_channel_still_stashes_the_reply() {
        let (bridge, peer) = test_bridge("quoin");
        let mut app = bus_app(bridge);
        // Sixteen connects fill the sixteen-slot outbound queue with snapshot
        // requests; the inbound work behind them then cannot be answered.
        for generation in 1..=16u64 {
            peer.deliver_event(BusBridgeEvent::Connection {
                state: BusConnectionState::Connected,
                generation,
            });
        }
        for _ in 0..4 {
            peer.send(request("shell.ping"));
        }
        app.update();
        assert_eq!(
            app.world()
                .resource::<ShellBusState>()
                .pending_replies
                .len(),
            4,
            "a full channel is retryable — the replies must be stashed"
        );
    }

    fn fill_outbound(peer: &ctk::bus::TestBusPeer) {
        for generation in 1..=16 {
            peer.deliver_event(BusBridgeEvent::Connection {
                state: BusConnectionState::Connected,
                generation,
            });
        }
    }

    fn drain_commands(app: &mut App) -> Vec<ShellCommand> {
        app.world_mut()
            .resource_mut::<bevy::ecs::message::Messages<ShellCommand>>()
            .drain()
            .collect()
    }

    #[test]
    fn a_full_channel_defers_reply_and_command_until_drain_exactly_once() {
        for verb in ["shell.quit", "shell.panel.toggle"] {
            let (bridge, peer) = test_bridge("quoin");
            let mut app = bus_app(bridge);
            fill_outbound(&peer);
            let request = local(verb);
            let expected = dispatch_shell_request(&request, &test_frame(), Default::default())
                .2
                .unwrap();
            peer.send(request);
            app.update();
            app.update(); // A retry while still full must not dispatch either.
            assert!(drain_commands(&mut app).is_empty());
            let state = app.world().resource::<ShellBusState>();
            assert_eq!(state.pending_replies.len(), 1);
            assert!(state.pending_replies[0].3.is_some());
            assert!(peer.drain_responses().is_empty()); // Free the full queue.

            app.update();
            let responses = peer.drain_responses();
            assert_eq!(responses.len(), 1);
            assert_eq!(responses[0].command, verb);
            assert_eq!(responses[0].rc, 0);
            let commands = drain_commands(&mut app);
            assert_eq!(commands.len(), 1);
            assert_eq!(commands[0].kind, expected.kind);
            assert!(
                app.world()
                    .resource::<ShellBusState>()
                    .pending_replies
                    .is_empty()
            );

            app.update();
            assert!(peer.drain_responses().is_empty());
            assert!(drain_commands(&mut app).is_empty());
        }
    }

    #[test]
    fn a_command_is_dispatched_only_after_its_reply_is_queued() {
        let (bridge, peer) = test_bridge("quoin");
        let request = local("shell.quit");
        let (rc, body, command) =
            dispatch_shell_request(&request, &test_frame(), Default::default());
        let mut state = ShellBusState::default();
        let mut dispatched = 0;
        stash_or_respond(
            &bridge,
            &mut state,
            request,
            rc,
            body,
            command,
            &mut |command| {
                // Inspect the actual outbound queue at the instant of dispatch.
                let responses = peer.drain_responses();
                assert_eq!(responses.len(), 1);
                assert_eq!(responses[0].command, "shell.quit");
                assert_eq!(responses[0].rc, 0);
                assert_eq!(command.kind, ShellCommandKind::Quit);
                dispatched += 1;
            },
        );
        assert_eq!(dispatched, 1);
        assert!(state.pending_replies.is_empty());
    }

    #[test]
    fn a_dead_worker_dispatches_the_command_while_dropping_the_reply() {
        let (bridge, peer) = test_bridge("quoin");
        peer.send(local("shell.quit"));
        drop(peer);
        let mut app = bus_app(bridge);
        app.update();
        let commands = drain_commands(&mut app);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].kind, ShellCommandKind::Quit);
        assert!(
            app.world()
                .resource::<ShellBusState>()
                .pending_replies
                .is_empty()
        );
        app.update();
        assert!(drain_commands(&mut app).is_empty());
    }

    #[test]
    fn a_stashed_command_survives_worker_loss_with_or_without_a_fatal_event() {
        for fatal_event in [false, true] {
            let (bridge, peer) = test_bridge("quoin");
            let mut app = bus_app(bridge);
            fill_outbound(&peer);
            peer.send(local("shell.quit"));
            app.update();
            assert!(drain_commands(&mut app).is_empty());
            if fatal_event {
                peer.deliver_event(BusBridgeEvent::Fatal("worker stopped".into()));
            }
            drop(peer);
            app.update();
            let commands = drain_commands(&mut app);
            assert_eq!(commands.len(), 1);
            assert_eq!(commands[0].kind, ShellCommandKind::Quit);
            assert!(
                app.world()
                    .resource::<ShellBusState>()
                    .pending_replies
                    .is_empty()
            );
            app.update();
            assert!(drain_commands(&mut app).is_empty());
        }
    }

    fn test_model() -> cosmix_shell::core::ShellModel {
        cosmix_shell::core::ShellModel::new(
            cosmix_shell::core::OutputKey::new("test").unwrap(),
            cosmix_shell::core::LogicalSize::new(1000.0, 800.0).unwrap(),
            Default::default(),
            std::time::Duration::from_millis(800),
            std::time::Duration::from_millis(200),
        )
        .unwrap()
    }

    fn test_frame() -> ShellFrame {
        ShellFrame::from_model(&test_model())
    }

    #[test]
    fn compatibility_props_and_legacy_unpin_confirm_release_of_both_modes() {
        use std::time::Duration;
        for (mode, transient) in [
            (PanelMode::Hidden, false),
            (PanelMode::Hidden, true),
            (PanelMode::Pinned, false),
            (PanelMode::Docked, false),
        ] {
            let mut model = test_model();
            model.set_mode(Edge::Left, Duration::ZERO, mode).unwrap();
            if transient {
                model
                    .panel_input(Edge::Left, Duration::ZERO, PanelInput::Reveal)
                    .unwrap();
            }
            let at = Duration::from_millis(200);
            model.tick(at).unwrap();
            let props = |model: &cosmix_shell::core::ShellModel| {
                let mut request = request("shell.props.get");
                request.body = json!({"path":"panels.left"}).to_string();
                let (rc, body, command) = dispatch_shell_request(
                    &request,
                    &ShellFrame::from_model(model),
                    model.last_update(),
                );
                assert_eq!(rc, 0);
                assert!(command.is_none());
                serde_json::from_str::<Value>(&body).unwrap()
            };
            let before = props(&model);
            assert_eq!(before["pinned"], json!(mode != PanelMode::Hidden));
            assert_eq!(before["mode"], json!(mode.as_str()));
            assert_eq!(
                before["visible"],
                json!(mode != PanelMode::Hidden || transient)
            );
            let (rc, body, command) = dispatch_shell_request(
                &local("shell.panel.unpin"),
                &ShellFrame::from_model(&model),
                at,
            );
            assert_eq!(rc, 0);
            assert_eq!(
                serde_json::from_str::<Value>(&body).unwrap(),
                json!({"accepted":true})
            );
            assert_eq!(
                props(&model),
                before,
                "enqueue acceptance has not applied anything"
            );
            let ShellCommandKind::Panel { edge, input } = command.unwrap().kind else {
                panic!("panel")
            };
            model.panel_input(edge, at, input).unwrap();
            assert_eq!(model.panel(edge).exclusive_zone_px, 0.0);
            assert_eq!(props(&model)["pinned"], json!(false));
            assert_eq!(props(&model)["mode"], json!("hidden"));
            model.panel_input(edge, at, PanelInput::Hide).unwrap();
            model.tick(Duration::from_millis(400)).unwrap();
            let confirmed = props(&model);
            assert_eq!(confirmed["pinned"], json!(false));
            assert_eq!(confirmed["mode"], json!("hidden"));
            assert_eq!(confirmed["visible"], json!(false));
        }
    }

    /// Chunk-2 acceptance gate: every mode-changing cell of the shell
    /// design's §3.1 transition table (the LMB and Shift+LMB columns; RMB
    /// opens the menu and changes nothing here, the drag gestures are
    /// deferred), walked twice — once as the raw corner-click input each
    /// gesture decodes to, once through the precise Bus verbs — asserting
    /// the APPLIED state, not the verb round-trip: resulting mode, the
    /// reservation that mode owes (only `Docked` claims an exclusive zone,
    /// and it claims its full thickness), and the persistence effect. The
    /// verb rows also read the applied mode back through the
    /// `panels.<edge>.mode` leaf.
    #[test]
    fn every_s31_mode_cell_applies_mode_and_reservation_via_verbs_and_clicks() {
        use std::time::Duration;
        let at = Duration::from_millis(100);
        let lmb = PanelInput::PinToggle;
        let shift_lmb = PanelInput::DockToggle;
        let mode_verb = |mode: &str| ("shell.panel.mode", json!({"edge":"left","mode":mode}));
        let dock_verb = || ("shell.panel.dock", json!({"edge":"left"}));
        let cells = [
            // hidden (incl. transiently revealed): LMB → pinned, Shift+LMB → docked
            (PanelMode::Hidden, false, lmb, mode_verb("pinned"), PanelMode::Pinned),
            (PanelMode::Hidden, true, lmb, mode_verb("pinned"), PanelMode::Pinned),
            (PanelMode::Hidden, false, shift_lmb, dock_verb(), PanelMode::Docked),
            (PanelMode::Hidden, true, shift_lmb, dock_verb(), PanelMode::Docked),
            // pinned: LMB → hidden, Shift+LMB → docked
            (PanelMode::Pinned, false, lmb, mode_verb("hidden"), PanelMode::Hidden),
            (PanelMode::Pinned, false, shift_lmb, dock_verb(), PanelMode::Docked),
            // docked: LMB → pinned, Shift+LMB → hidden
            (PanelMode::Docked, false, lmb, mode_verb("pinned"), PanelMode::Pinned),
            (PanelMode::Docked, false, shift_lmb, mode_verb("hidden"), PanelMode::Hidden),
        ];
        for (start, transient, click, (verb, body), target) in cells {
            let context = format!("{start:?} + {click:?} → {target:?}");
            // Click path: the gesture's decoded input, held and unheld — the
            // corner holds the panel while the pointer rests in it.
            for held in [false, true] {
                let mut model = test_model();
                if start != PanelMode::Hidden {
                    model.set_mode(Edge::Left, Duration::ZERO, start).unwrap();
                } else if transient {
                    model
                        .panel_input(Edge::Left, Duration::ZERO, PanelInput::Reveal)
                        .unwrap();
                }
                if held {
                    model
                        .panel_input(Edge::Left, at, PanelInput::CornerEntered)
                        .unwrap();
                }
                let update = model.panel_input(Edge::Left, at, click).unwrap();
                assert_applied_mode(&model, target, &context);
                assert_eq!(
                    update.effect,
                    Some(cosmix_shell::core::PanelEffect::ModeChanged { mode: target }),
                    "{context} (held={held})"
                );
                // Chunk 3's arm: a deliberate undock from Docked hides at
                // once only when nothing holds the panel; held keeps the
                // transient reveal (§4.3 — grace never applies to the
                // deliberate action itself, but the holder still holds).
                if start == PanelMode::Docked && click == PanelInput::DockToggle {
                    let snapshot = model.panel(Edge::Left);
                    assert_eq!(snapshot.transient_revealed, held, "{context}");
                    assert_eq!(snapshot.hide_at, None, "{context}");
                }
            }

            // Verb path: the precise Bus verb, applied to the model and read
            // back through the props leaf.
            let mut model = test_model();
            if start != PanelMode::Hidden {
                model.set_mode(Edge::Left, Duration::ZERO, start).unwrap();
            } else if transient {
                model
                    .panel_input(Edge::Left, Duration::ZERO, PanelInput::Reveal)
                    .unwrap();
            }
            let verb_request = wire(verb, body);
            let (rc, reply, command) = dispatch_shell_request(
                &verb_request,
                &ShellFrame::from_model(&model),
                at,
            );
            assert_eq!(rc, 0, "{context}: {reply}");
            let ShellCommandKind::Panel {
                edge,
                input: panel_input,
            } = command.expect("accepted verb enqueues a command").kind
            else {
                panic!("{context}: panel command");
            };
            let update = model.panel_input(edge, at, panel_input).unwrap();
            assert_applied_mode(&model, target, &context);
            assert_eq!(
                update.effect,
                Some(cosmix_shell::core::PanelEffect::ModeChanged { mode: target }),
                "{context}"
            );
            // A deliberate Hide through the mode verb conceals at once: no
            // transient reveal survives, no grace deadline is armed.
            if target == PanelMode::Hidden {
                let snapshot = model.panel(Edge::Left);
                assert!(!snapshot.transient_revealed, "{context}");
                assert_eq!(snapshot.hide_at, None, "{context}");
            }
            let mut props = request("shell.props.get");
            props.body = json!({"path":"panels.left.mode"}).to_string();
            let (rc, leaf, _) = dispatch_shell_request(
                &props,
                &ShellFrame::from_model(&model),
                model.last_update(),
            );
            assert_eq!(rc, 0);
            assert_eq!(
                serde_json::from_str::<Value>(&leaf).unwrap(),
                json!(target.as_str()),
                "{context}: the mode leaf must report the applied mode"
            );
        }

        fn assert_applied_mode(
            model: &cosmix_shell::core::ShellModel,
            target: PanelMode,
            context: &str,
        ) {
            let snapshot = model.panel(Edge::Left);
            assert_eq!(snapshot.mode, target, "{context}");
            if target == PanelMode::Docked {
                // Docked reserves its full thickness (shell doc §3).
                assert!(snapshot.exclusive_zone_px > 0.0, "{context}");
                assert_eq!(
                    snapshot.exclusive_zone_px, snapshot.thickness_px,
                    "{context}"
                );
            } else {
                // Hidden and Pinned reserve nothing; a transient reveal
                // never reads as a reservation.
                assert_eq!(snapshot.exclusive_zone_px, 0.0, "{context}");
            }
        }
    }

    /// A frame whose carousels carry Quoin's real page schema.
    ///
    /// [`test_frame`]'s carousels are empty, which makes every page id
    /// unknown and every `page.set` refusable — a frame in which the
    /// accepting branch is unreachable, so no assertion written against it
    /// can distinguish a working verb from a broken one.
    fn paged_frame() -> ShellFrame {
        let mut model = test_model();
        for (edge, pages) in [
            (Edge::Left, ["nav", "places"].as_slice()),
            (Edge::Bottom, ["launcher", "power", "tasks"].as_slice()),
            (Edge::Right, ["monitor", "agents"].as_slice()),
            (Edge::Top, ["status", "spaces"].as_slice()),
        ] {
            model.set_carousel(
                edge,
                cosmix_shell::core::Carousel::new(pages.iter().copied())
                    .expect("static test page schema is valid"),
            );
        }
        ShellFrame::from_model(&model)
    }
}
