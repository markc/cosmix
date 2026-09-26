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

/// Bound on replies stashed while the outbound channel is full. Beyond this
/// the oldest is dropped with a warning — a bounded stash that eventually
/// answers beats an unbounded one, and both beat silently losing every reply
/// the moment the channel blinks.
const MAX_PENDING_REPLIES: usize = 32;
const RESIZE_RECEIPT_FRAMES: u64 = 120;

#[derive(Resource)]
struct ShellBusState {
    diagnostics: BusDiagnostics,
    ready_logged: bool,
    next_request_id: u64,
    /// Current Bus connection epoch; rejects stale requests and observations.
    live_generation: Option<u64>,
    /// Replies that hit a full outbound channel, retried before new inbound
    /// work. Losing a reply outright would leave the peer hanging until its
    /// own timeout — worse than answering late.
    pending_replies: Vec<(InboundRequest, u8, String, Option<ShellCommand>)>,
    pending_resizes: BTreeMap<u64, (InboundRequest, u64)>,
    /// Panel operations answered from Presentation, after Model. Concealment
    /// uses the host's existing animation frames, never a Bus notification.
    pending_panels: Vec<(InboundRequest, cosmix_shell::core::OutputKey)>,
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
    applied_panels: Value,
    panel_revision: u64,
    /// Shadow of the scheme selection for the settings snapshot; seeded
    /// lazily from the persisted state (see `settings::initial_scheme`).
    settings_scheme: Option<String>,
    applied_settings: Value,
    settings_revision: u64,
}

impl Default for ShellBusState {
    fn default() -> Self {
        Self {
            diagnostics: BusDiagnostics::default(),
            ready_logged: false,
            next_request_id: 0x51_0000_0000,
            live_generation: None,
            pending_replies: Vec::new(),
            pending_resizes: BTreeMap::new(),
            pending_panels: Vec::new(),
            citizen_receipt: 0,
            disconnected_citizens: BTreeMap::new(),
            citizen_snapshot: None,
            citizen_snapshot_retry: false,
            frame: 0,
            applied_panels: Value::Null,
            panel_revision: 0,
            settings_scheme: None,
            applied_settings: Value::Null,
            settings_revision: 0,
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
            .init_resource::<cosmix_shell::chrome::QuoinHotspotSize>()
            .init_resource::<SubPanelRegistryState>()
            .init_resource::<cosmix_scene_bevy::SceneStore>()
            .init_resource::<cosmix_scene_bevy::SceneEvents>()
            .init_resource::<crate::config::ShellConfig>()
            .add_message::<cosmix_shell::chrome::QuoinSchemeSelected>()
            // Holder reporting still arms retry deadlines after the native
            // pages are gone. Embedded hosts also run report_holders, even
            // without a HolderClient, so its required resource lives here.
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
            .add_systems(Update, reply_resizes.in_set(ShellRuntimeSet::Presentation))
            .add_systems(Update, reply_panels.in_set(ShellRuntimeSet::Presentation))
            .add_systems(Update, publish_panel_state.in_set(ShellRuntimeSet::Presentation))
            .add_systems(Update, publish_settings_state.in_set(ShellRuntimeSet::Presentation))
            .add_systems(
                Update,
                crate::holders::report_holders
                    .in_set(ShellRuntimeSet::Presentation)
                    .after(service_bus),
            );
    }
}

/// Publish the applied frame, after Model and scene reconciliation, rather
/// than command enqueue acceptance. No timer and no idle publication.
fn publish_panel_state(
    bridge: Res<BusBridge>,
    (frame, config): (Res<ShellFrameState>, Res<crate::config::ShellConfig>),
    mut state: ResMut<ShellBusState>,
    dialog: Option<Res<cosmix_shell::chrome::dialog::QuoinDialog>>,
) {
    if state.live_generation.is_none() { return; }
    let mut panels = panel_notice_snapshot(&frame.0, &config.panels);
    // Compared with the panels: a dialog-only change is a new revision.
    panels["dialog"] = crate::dialog_bus::notice(dialog.as_deref());
    if panels == state.applied_panels { return; }
    let revision = state.panel_revision.saturating_add(1);
    let mut body = panels.clone();
    body["generation"] = json!(state.live_generation);
    body["revision"] = json!(revision);
    let wire = format!("---\ncommand: shell.panel.changed\n---\n{body}");
    let topic = format!("{}.panel.changed", bridge.service_name());
    if bridge.try_publish_topic(&topic, true, wire).is_ok() {
        state.applied_panels = panels;
        state.panel_revision = revision;
    }
}

/// `shell.settings.changed`: the settings snapshot, published when it differs
/// from the last one (a scheme selected from chrome or the Bus, an ingested
/// motion, a settled resize, the page changing hands). Same shape as the
/// `shell.settings.get` reply. No timer and no idle publication.
fn publish_settings_state(
    bridge: Res<BusBridge>,
    frame: Res<ShellFrameState>,
    (config, registry): (Res<crate::config::ShellConfig>, Res<SubPanelRegistryState>),
    store: Option<Res<crate::state::StateStore>>,
    mut selections: MessageReader<cosmix_shell::chrome::QuoinSchemeSelected>,
    mut state: ResMut<ShellBusState>,
) {
    let scheme = state
        .settings_scheme
        .get_or_insert_with(|| crate::settings::initial_scheme(store.as_deref()));
    for selection in selections.read() {
        *scheme = selection.0.name().to_owned();
    }
    let snapshot = crate::settings::snapshot(
        scheme,
        &config,
        &frame.0,
        settings_page_owner(&registry.0),
    );
    if state.live_generation.is_none() || snapshot == state.applied_settings {
        return;
    }
    let revision = state.settings_revision.saturating_add(1);
    let mut body = snapshot.clone();
    body["generation"] = json!(state.live_generation);
    body["revision"] = json!(revision);
    let wire = format!("---\ncommand: shell.settings.changed\n---\n{body}");
    let topic = format!("{}.settings.changed", bridge.service_name());
    if bridge.try_publish_topic(&topic, true, wire).is_ok() {
        state.applied_settings = snapshot;
        state.settings_revision = revision;
    }
}

/// Who serves `settings.appearance` right now (`quoin@host` = the built-in).
fn settings_page_owner(registry: &cosmix_shell::core::SubPanelRegistry) -> Option<&str> {
    registry
        .seat(crate::config::SETTINGS_APPEARANCE)
        .map(|seat| seat.owner.as_str())
}

/// Selection/mode/mapping are discrete state. During a resize gesture retain
/// the settled width; publish the final size once, without serialising the
/// entire property tree on every animation frame. `visible` is a boolean,
/// so reveal/conceal emits only its mapping transitions, not motion fractions.
/// `declared` (the `conf.mix` order) and `dialog` are part of the compared
/// snapshot, so a `shell.panel.order` ingestion or a dialog change publishes
/// with a new revision (scene-editor plan §4.3).
fn panel_notice_snapshot(frame: &ShellFrame, declared: &[Vec<String>; 4]) -> Value {
    let mut panels = serde_json::Map::new();
    for edge in Edge::ALL {
        let panel = frame.panel(edge);
        panels.insert(edge_name(edge).into(), json!({
            "visible": panel.mapped,
            "pinned": panel.mode != PanelMode::Hidden,
            "mode": panel.mode.as_str(),
            "width_px": panel.settled_thickness_px,
            "page": panel.active_page_id,
            "pages": panel.page_ids.as_ref(),
            "declared": declared[edge.index()],
            "output": frame.geometry.output.as_str(),
        }));
    }
    // `publish_panel_state` fills in the dialog seat (dialog_bus::notice).
    json!({"dialog": Value::Null, "panels": panels})
}

/// These idempotent verbs drive the legacy citizen's select/pin/release
/// sequence. A superseding command or output change is an explicit refusal,
/// not a success inferred from enqueueing. Reads remain immediate snapshots.
fn reply_panels(
    bridge: Res<BusBridge>,
    (frame, config): (Res<ShellFrameState>, Res<crate::config::ShellConfig>),
    mut state: ResMut<ShellBusState>,
) {
    for (request, output) in std::mem::take(&mut state.pending_panels) {
        if state.live_generation != Some(request.connection_generation) {
            continue;
        }
        let edge = argument(&request, "edge").and_then(parse_edge).expect("validated edge");
        let panel = frame.0.panel(edge);
        let applied = output == frame.0.geometry.output && match request.command.as_str() {
            "shell.panel.page.set" => panel.active_page_id == argument(&request, "id"),
            "shell.panel.pin" => panel.mode == PanelMode::Docked && panel.mapped,
            "shell.panel.mode" => Some(panel.mode.as_str().to_owned()) == argument(&request, "mode"),
            _ => unreachable!("only applied panel verbs are queued"),
        };
        // Hidden mode is applied before its outgoing motion completes. The
        // model already requests frames until unmapping; hold the reply until
        // that state is observable, so even a dropped final notice is harmless.
        if applied && request.command == "shell.panel.mode"
            && panel.mode == PanelMode::Hidden && panel.mapped
            && !panel.transient_revealed
        {
            state.pending_panels.push((request, output));
            continue;
        }
        // A pointer/holder reveal does not undo the applied persistent mode.
        // Report its actual visible:true state instead of a false refusal.
        let snapshot = Value::from(&ShellProps(&frame.0, &config.panels, PropValue::Null).snapshot());
        let body = if applied {
            json!({"accepted":true, "applied":true, "panels":snapshot["panels"]})
        } else {
            json!({"error_code":"PANEL_NOT_APPLIED", "message":"panel command was superseded or could not apply", "panels":snapshot["panels"]})
        };
        stash_or_respond(&bridge, &mut state, request, if applied { 0 } else { 10 },
            body.to_string(), None, &mut |_| {});
    }
}

#[derive(bevy::ecs::system::SystemParam)]
struct SceneBus<'w> {
    holders: Option<ResMut<'w, crate::holders::HolderClient>>,
    targets: Option<ResMut<'w, crate::activation::ActivationTargets>>,
    scenes: ResMut<'w, cosmix_scene_bevy::SceneStore>,
    events: ResMut<'w, cosmix_scene_bevy::SceneEvents>,
    registry: ResMut<'w, SubPanelRegistryState>,
    config: ResMut<'w, crate::config::ShellConfig>,
    schemes: MessageWriter<'w, cosmix_shell::chrome::QuoinSchemeSelected>,
    settings: Option<ResMut<'w, crate::settings::SettingsScene>>,
    dialog: Option<ResMut<'w, crate::dialog_bus::DialogRequests>>,
    dialog_state: Option<Res<'w, cosmix_shell::chrome::dialog::QuoinDialog>>,
    order_writer: Option<Res<'w, crate::order_writer::OrderWriter>>,
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
    (frame, time): (Res<ShellFrameState>, Res<Time<Real>>),
    mut state: ResMut<ShellBusState>,
    mut shell_commands: MessageWriter<ShellCommand>,
    mut content: SceneBus,
    (mut hotspot, mut hotspot_size, state_store): (
        Option<ResMut<crate::hotspot::HotspotObserver>>,
        ResMut<cosmix_shell::chrome::QuoinHotspotSize>,
        Option<Res<crate::state::StateStore>>,
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

    if state.citizen_snapshot_retry {
        request_citizen_snapshot(&bridge, &mut state);
    }

    for event in bridge.drain_events() {
        if let Some(client) = content.holders.as_deref_mut() { client.event(&event); }
        if let Some(targets) = content.targets.as_deref_mut() { targets.event(&event); }
        if let Some(observer) = hotspot.as_deref_mut() {
            observer.event(&event, &mut hotspot_size);
        }
        content.events.reply(&event);
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
                state.applied_panels = Value::Null;
                state.applied_settings = Value::Null;
                request_citizen_snapshot(&bridge, &mut state);
            }
            BusBridgeEvent::Connection { .. } | BusBridgeEvent::Fatal(_) => {
                state.pending_resizes.clear();
                state.pending_panels.clear();
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
                            if let Some(client) = content.holders.as_deref_mut() { client.presence(&live); }
                            if let Some(observer) = hotspot.as_deref_mut() {
                                observer.presence(&live, &mut hotspot_size);
                            }
                        } else {
                            warn!("citizen registry snapshot failed; awaiting next Bus trigger");
                        }
                    }
                }
            }
            BusBridgeEvent::DroppedMessages(_) => {
                request_citizen_snapshot(&bridge, &mut state);
            }
            BusBridgeEvent::ObservationDroppedMessages(_) => {
                request_citizen_snapshot(&bridge, &mut state);
            }
            BusBridgeEvent::ObservationConnection { .. }
            | BusBridgeEvent::ObservationReply { .. } => {}
        }
    }
    // The model goes command-driven before the commands the open gate admits,
    // and back to local rules the moment the gate closes.
    let holder_plane = |client: &mut crate::holders::HolderClient,
                        commands: &mut MessageWriter<ShellCommand>| {
        if let Some(available) = client.plane_change() {
            commands.write(ShellCommand {
                output: frame.0.geometry.output.clone(),
                at: time.elapsed(),
                kind: ShellCommandKind::HolderPlane(available),
            });
        }
    };
    if let Some(client) = content.holders.as_deref_mut() {
        holder_plane(client, &mut shell_commands);
    }
    for message in bridge.drain_messages() {
        if let Some(client) = content.holders.as_deref_mut()
            && let Some(command) = client.message(&message)
            && let Some(command) = command.shell_command(time.elapsed())
        {
            shell_commands.write(command);
        }
        if let Some(observer) = hotspot.as_deref_mut() {
            observer.message(&message);
        }
        if let Some(targets) = content.targets.as_deref_mut() {
            targets.message(&message);
        }
        if state.live_generation == Some(message.connection_generation) {
            if let Some(live) = registered_services(&message) {
                // A departure seen on the topic cannot be fenced: the topic and
                // the request channel are not ordered, so this message may
                // drain after a restarted owner's fresh loads, and a drain-time
                // cutoff would drop them silently. Confirm it with a snapshot
                // instead, fenced at request time like every other snapshot.
                if content.registry.0.live_owners().difference(&live).next().is_some() {
                    request_citizen_snapshot(&bridge, &mut state);
                } else {
                    // Nobody left: this full observation supersedes any
                    // in-flight snapshot.
                    state.citizen_snapshot = None;
                    state.citizen_snapshot_retry = false;
                }
                if let Some(client) = content.holders.as_deref_mut() { client.presence(&live); }
                if let Some(observer) = hotspot.as_deref_mut() {
                    observer.presence(&live, &mut hotspot_size);
                }
            } else if message
                .headers
                .get("gap")
                .is_some_and(|value| value == "true")
            {
                request_citizen_snapshot(&bridge, &mut state);
            }
        }
    }
    if let Some(client) = content.holders.as_deref_mut() {
        holder_plane(client, &mut shell_commands);
    }
    if let Some(targets) = content.targets.as_deref_mut() { targets.flush(&bridge); }
    if let Some(observer) = hotspot.as_deref_mut() {
        observer.flush(&bridge);
        // Comp accepted the first-run discovery write: never request it again.
        if observer.take_first_run_written()
            && let Some(store) = state_store.as_deref()
        {
            store.consume_first_run();
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
        let (rc, body, command) =
            if let Some(verb) = cosmix_shell::runtime::SceneVerb::parse(&request.command) {
                let args = parse_args(&request).unwrap_or(Value::Null);
                let (rc, body) = if let Err(error) = verify_caller_provenance(&request) {
                    (
                        10,
                        json!({"error_code":"SCENE_PROVENANCE", "message":format!("scene caller provenance: {error:?}")}).to_string(),
                    )
                } else if state
                    .live_generation
                    .is_some_and(|generation| generation != request.connection_generation)
                {
                    (
                        10,
                        json!({"error_code":"SCENE_STALE_CONNECTION", "message":"scene request belongs to a stale Quoin connection"})
                            .to_string(),
                    )
                } else {
                    state.citizen_receipt = state
                        .citizen_receipt
                        .checked_add(1)
                        .expect("receipt sequence exhausted");
                    let owner = attested_owner(&request, state.citizen_receipt);
                    let SceneBus {
                        scenes, registry, settings, ..
                    } = &mut content;
                    // The loader's settings template replaces the built-in
                    // fallback page in this same dispatch (settings.rs).
                    let refusal = if verb == cosmix_shell::runtime::SceneVerb::Load {
                        crate::settings::yield_to_external(
                            &request.body,
                            &args,
                            settings.as_deref_mut(),
                            scenes,
                            &mut registry.0,
                            &frame.0.geometry.output,
                            &bridge,
                        )
                    } else {
                        None
                    };
                    match refusal {
                        Some(refusal) => refusal,
                        None => scenes.dispatch(
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
                        ),
                    }
                };
                (rc, body, None)
            } else if crate::dialog_bus::handles(&request.command) {
                // Scene Editor plan §4.3 Q2: dialog and layout verbs live in
                // dialog_bus.rs, answered with world access in the Model stage.
                // Fixtures that install the Bus service alone have no queue.
                let Some(queue) = content.dialog.as_deref_mut() else {
                    let body = json!({"error_code":"UNIMPLEMENTED", "message":"dialog verbs are not installed in this host"});
                    stash_or_respond(&bridge, &mut state, request, 10, body.to_string(), None, &mut dispatch);
                    continue;
                };
                queue.defer(request);
                continue;
            } else if request.command == "shell.panel.order" {
                // Scene Editor plan §4.3 Q1; request/reply frozen in
                // tests/fixtures/scene-editor/shell-verbs.json. The same
                // stale-connection fence as the settings writes: a stale
                // request must not spend a conf.mix rewrite.
                let (rc, body) = if state
                    .live_generation
                    .is_some_and(|generation| generation != request.connection_generation)
                {
                    (
                        10,
                        json!({"error_code":"STALE_CONNECTION", "message":"panel.order request belongs to a stale Quoin connection"}),
                    )
                } else if let Some(writer) = content.order_writer.as_deref()
                    && writer.submit(request.clone(), crate::config::conf_mix_path())
                {
                    // Written and answered off the render thread (order_writer.rs).
                    continue;
                } else {
                    panel_order(&request.body, &crate::config::conf_mix_path())
                };
                (rc, body.to_string(), None)
            } else if request.command == "shell.scenes.list" {
                (
                    0,
                    content
                        .scenes
                        .list(&content.registry.0, &frame.0.geometry.output)
                        .to_string(),
                    None,
                )
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
            } else if request.command == "shell.sub.activate" {
                // Gated on the gate the model follows: the plane change above
                // already reached it, so an admitted activation's reveal is
                // command-driven and held by comp, never left to local grace.
                // Wontfix (review NIT-5): the gate can close in the one frame
                // between this check and the Model stage; the reveal then falls
                // to local rules and stays up through its explicit-show flag
                // until a hide, as `shell.panel.show` does. Closing that would
                // mean deferring the reply to the Model stage for a window that
                // only a comp gap or restart opens, and a panel left open is the
                // safe side of it (never one that vanishes while typed into).
                crate::activation::dispatch_activate(
                    &request,
                    &frame.0,
                    &content.registry.0,
                    content
                        .holders
                        .as_deref()
                        .is_some_and(|client| client.capable),
                    content
                        .targets
                        .as_deref()
                        .and_then(|targets| targets.target()),
                    state.live_generation,
                    time.elapsed(),
                )
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
                        config, schemes, registry, ..
                    } = &mut content;
                    let scheme = state.settings_scheme.get_or_insert_with(|| {
                        crate::settings::initial_scheme(state_store.as_deref())
                    });
                    if request.command == "shell.settings.get" {
                        // A read: the snapshot the template behaviour builds
                        // its model from; `revision` is the last notice's
                        // while the body is live, so it may be newer than
                        // that notice (documented in docs/cos/quoin.md).
                        let mut body = crate::settings::snapshot(
                            scheme,
                            config,
                            &frame.0,
                            settings_page_owner(&registry.0),
                        );
                        body["generation"] = json!(state.live_generation);
                        body["revision"] = json!(state.settings_revision);
                        (0, body.to_string(), None)
                    } else {
                        crate::settings::dispatch_verb(
                            &request,
                            &frame.0,
                            config,
                            &crate::config::conf_mix_path(),
                            schemes,
                            scheme,
                            time.elapsed(),
                        )
                    }
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
                dispatch_with_declared(
                    &request,
                    &frame.0,
                    &content.config.panels,
                    &crate::dialog_bus::notice_prop(content.dialog_state.as_deref()),
                    time.elapsed(),
                )
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
        if rc == 0 && matches!(request.command.as_str(),
            "shell.panel.page.set" | "shell.panel.pin" | "shell.panel.mode")
            && let Some(command) = &command
        {
            if state.pending_panels.len() < MAX_PENDING_REPLIES {
                state.pending_panels.push((request, command.output.clone()));
                dispatch(command.clone());
            } else {
                stash_or_respond(&bridge, &mut state, request, 11,
                    json!({"error":"panel queue full"}).to_string(), None, &mut dispatch);
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
    // Announce every departure unload now (reason "owner_departed"), not at
    // the next scene request: an owner that is in fact back hears that its
    // scene is gone and remounts it.
    if world.contains_resource::<cosmix_scene_bevy::SceneStore>() && world.contains_resource::<BusBridge>() {
        world.resource_scope(|world, mut store: Mut<cosmix_scene_bevy::SceneStore>| {
            store.publish_notices(world.resource::<BusBridge>());
        });
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

/// The sub-panel lifecycle verbs (panel doc §3): `sub.register` and
/// `sub.remove` — activation is a separate verb, gated on the compositor
/// ([`crate::activation::dispatch_activate`]).
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

/// `shell.panel.order {edges:{<edge>:[string], …}}` → `{edges}` as written.
/// One atomic `conf.mix` replacement; the file watcher ingests it like a
/// hand edit, so a later hand edit still wins. Mesh-open: what stays is
/// well-formedness (edges, identifiers, no page on two edges).
pub(crate) fn panel_order(body: &str, path: &std::path::Path) -> (u8, Value) {
    let order = serde_json::from_str::<Value>(body)
        .map_err(|error| crate::config::OrderRefusal {
            code: "INVALID_ARGUMENT",
            message: format!("body is not JSON: {error}"),
            edges: Vec::new(),
        })
        .and_then(|body| crate::config::parse_panel_order(&body));
    match order.and_then(|order| crate::config::write_panel_order(path, &order).map(|()| order)) {
        Ok(order) => {
            let edges: serde_json::Map<String, Value> = order
                .into_iter()
                .map(|(edge, pages)| (edge_name(edge).to_owned(), json!(pages)))
                .collect();
            (0, json!({"edges": edges}))
        }
        Err(refusal) => (10, refusal.body()),
    }
}

/// [`dispatch_with_declared`] for a host with no `conf.mix` declarations.
#[cfg(test)]
fn dispatch_shell_request(
    request: &InboundRequest,
    frame: &ShellFrame,
    at: std::time::Duration,
) -> (u8, String, Option<ShellCommand>) {
    dispatch_with_declared(request, frame, &NO_DECLARED, &PropValue::Null, at)
}

/// `declared` is the last accepted `conf.mix` page order per edge, which the
/// props tree reports as `panels.<edge>.declared`.
fn dispatch_with_declared(
    request: &InboundRequest,
    frame: &ShellFrame,
    declared: &[Vec<String>; 4],
    dialog: &PropValue,
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
            "verbs":["quit","panel.show","panel.hide","panel.toggle","panel.pin","panel.unpin","panel.dock","panel.mode","panel.resize","panel.order","panel.state","panel.page.next","panel.page.prev","panel.page.set","sub.register","sub.remove","sub.activate","settings.scheme","settings.motion","settings.size","settings.get","corner.show","corner.hide","corner.toggle","corner.pin","corner.unpin","debug.status","scene.load","scene.validate","scene.patch","scene.get","scene.describe","scene.unload","scene.watch","scene.layout","scenes.list","dialog.show","dialog.hide"],
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
            &ShellProps(frame, declared, dialog.clone()),
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
    if request.command == "shell.panel.state" {
        let Some(edge) = argument(request, "edge").and_then(parse_edge) else {
            return (
                10,
                json!({"error":"edge must be left, bottom, right or top"}).to_string(),
                None,
            );
        };
        let snapshot = Value::from(&ShellProps(frame, declared, PropValue::Null).snapshot());
        let mut state = snapshot["panels"][edge_name(edge)].clone();
        let panel = frame.panel(edge);
        state["keyboard_focused"] = json!(panel.keyboard_focused);
        state["keyboard_requested"] = json!(panel.keyboard_requested);
        return (0, state.to_string(), None);
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
        // One per-orientation range shared with the settings stepper and
        // pointer drag (scene-editor plan §4.3 Q1).
        let range = cosmix_shell::core::resize_thickness_range(edge);
        let Some(thickness_px) = number_argument(request, "thickness_px")
            .map(|value| value as f32)
            .filter(|value| range.contains(value))
        else {
            return (
                10,
                json!({
                    "error": format!(
                        "thickness_px must be a number in {}..={} for the {} edge",
                        range.start(), range.end(), edge_name(edge)
                    ),
                    "range_px": [range.start(), range.end()],
                })
                .to_string(),
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
    if frame.empty_edges_suppressed
        && frame.panel(edge).page_ids.is_empty()
        && matches!(&command.kind, ShellCommandKind::Panel { input, .. } if input.requires_content())
    {
        return (
            10,
            // Unified refusal shape (decision 10): {error_code, message}.
            json!({"error_code":"EMPTY_EDGE", "message":"edge has no registered pages", "edge":edge_name(edge)}).to_string(),
            None,
        );
    }
    // The dispatcher describes enqueue acceptance. service_bus upgrades
    // page.set/pin/mode to applied receipts in Presentation; other legacy
    // verbs retain their acceptance reply and require state readback.
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

/// No `conf.mix` declarations: what a host without the config reader reports.
#[cfg(test)]
const NO_DECLARED: [Vec<String>; 4] = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];

/// The live frame plus the last accepted `conf.mix` page order per edge
/// (`ShellConfig::panels`, indexed by `Edge::index`).
/// The third field is the dialog seat (`dialog_bus::notice_prop`).
struct ShellProps<'a>(&'a ShellFrame, &'a [Vec<String>; 4], PropValue);

impl PropTree for ShellProps<'_> {
    fn snapshot(&self) -> PropValue {
        // The one dialog seat (scene-editor plan §4.3): null until a dialog
        // scene is loaded.
        let mut leaves = vec![leaf("dialog".to_owned(), self.2.clone())];
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
                    format!("panels.{name}.declared"),
                    self.1[edge.index()].clone().into(),
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
        let mut paths = vec![PropPath::new("dialog".to_owned()).unwrap()];
        for edge in Edge::ALL {
            for field in ["visible", "pinned", "mode", "width_px", "page", "pages", "declared", "output"] {
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
            "pages" | "declared" => PropType::List,
            "dialog" => PropType::Object,
            _ => return None,
        };
        Some(PropDescribe::leaf(
            path.clone(),
            ty,
            match field {
                "declared" => "the conf.mix page order for this edge (shell.panel.order writes it)",
                "dialog" => {
                    "the dialog seat {scene, visible, w, h, output}, or null when no dialog scene is loaded"
                }
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

pub(crate) fn edge_name(edge: Edge) -> &'static str {
    edge.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire contract against a non-default comp: literal `comp.*`
    /// commands addressed to that service. A menu closed while the Bus was
    /// down is still released after reconnect (comp kept the hold), and
    /// once acknowledged its token goes quiet.
    #[test]
    fn client_sends_hold_on_popup_open() {
        let (bridge, peer) = ctk::bus::test_bridge("shell");
        let mut app = bus_app(bridge);
        let mut bus = ctk::bus::BusBridgeConfig::new("shell", "ws://127.0.0.1:9000");
        crate::holders::install(&mut app, &mut bus, "comp-nested".into());
        assert!(bus.subscriptions.contains(&"comp-nested.panel.command".into()));
        app.insert_resource(cosmix_shell_host::holders::PanelLayerIdentities(vec![(
            test_model().output().clone(), Edge::Left, "panel-token".into(),
        )]));
        app.insert_resource(cosmix_shell_host::holders::PopupLayerIdentity {
            output: test_model().output().clone(), edge: Edge::Left, surface: "menu-token".into(),
        });
        let reply = |request_id, body: &str| BusBridgeEvent::Reply {
            request_id, result: Ok(ctk::bus::BusReply { rc: 0, body: body.into(), result: None }),
        };
        let comp_calls = || -> Vec<_> {
            peer.drain_calls().into_iter().filter(|call| call.command.starts_with("comp")).collect()
        };
        let menu = |app: &mut App, open: bool| {
            app.world_mut().write_message(ShellCommand {
                output: test_model().output().clone(), at: Default::default(),
                kind: ShellCommandKind::Panel { edge: Edge::Left,
                    input: cosmix_shell::core::PanelInput::MenuHold(open) },
            });
        };
        // Chunk 15: mode reports carry the Bus connection generation.
        let mode = json!({"output":"test","edge":"left","surface":"panel-token","mode":"hidden",
            "generation":1});
        let hold = |acquire: bool| json!({"output":"test","edge":"left","surface":"menu-token",
            "holder":"popup","acquire":acquire});
        peer.deliver_event(BusBridgeEvent::Connection {
            state: BusConnectionState::Connected, generation: 1,
        });
        menu(&mut app, true);
        app.update();
        let calls = comp_calls();
        assert_eq!(calls.len(), 1, "only the capability read before capability");
        assert_eq!((calls[0].to.as_str(), calls[0].command.as_str()), ("comp-nested", "comp.props.get"));
        assert_eq!(calls[0].body, r#"{"path":"input.corners.holders"}"#);
        peer.deliver_event(reply(calls[0].request_id, "true"));
        app.update();
        let calls = comp_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!((calls[0].to.as_str(), calls[0].command.as_str()), ("comp-nested", "comp.panel.mode"));
        assert_eq!(serde_json::from_str::<Value>(&calls[0].body).unwrap(), mode);
        peer.deliver_event(reply(calls[0].request_id, r#"{"accepted":true}"#));
        app.update();
        let calls = comp_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!((calls[0].to.as_str(), calls[0].command.as_str()), ("comp-nested", "comp.panel.hold"));
        assert_eq!(serde_json::from_str::<Value>(&calls[0].body).unwrap(), hold(true));
        peer.deliver_event(reply(calls[0].request_id, r#"{"accepted":true}"#));
        // The Bus drops; the menu closes meanwhile.
        peer.deliver_event(BusBridgeEvent::Connection {
            state: BusConnectionState::Disconnected, generation: 1,
        });
        app.world_mut().remove_resource::<cosmix_shell_host::holders::PopupLayerIdentity>();
        menu(&mut app, false);
        app.update();
        assert!(comp_calls().is_empty(), "nothing is sent while disconnected");
        peer.deliver_event(BusBridgeEvent::Connection {
            state: BusConnectionState::Connected, generation: 2,
        });
        app.update();
        let calls = comp_calls();
        assert_eq!(calls[0].command, "comp.props.get");
        peer.deliver_event(reply(calls[0].request_id, "true"));
        app.update();
        let calls = comp_calls();
        assert_eq!(calls[0].command, "comp.panel.mode", "mode reports replay first");
        peer.deliver_event(reply(calls[0].request_id, r#"{"accepted":true}"#));
        app.update();
        let calls = comp_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(serde_json::from_str::<Value>(&calls[0].body).unwrap(), hold(false));
        peer.deliver_event(reply(calls[0].request_id, r#"{"accepted":true}"#));
        app.update();
        app.update();
        assert!(comp_calls().is_empty(), "an acknowledged release is never replayed");
    }

    /// Comp's registration lapses while comp itself (and its hold) lives on.
    /// The menu closing during that outage is still released on return.
    #[test]
    fn hold_is_released_after_comp_registration_outage() {
        let (bridge, peer) = ctk::bus::test_bridge("shell");
        let mut app = bus_app(bridge);
        let mut bus = ctk::bus::BusBridgeConfig::new("shell", "ws://127.0.0.1:9000");
        crate::holders::install(&mut app, &mut bus, "comp-nested".into());
        app.insert_resource(cosmix_shell_host::holders::PanelLayerIdentities(vec![(
            test_model().output().clone(), Edge::Left, "panel-token".into(),
        )]));
        app.insert_resource(cosmix_shell_host::holders::PopupLayerIdentity {
            output: test_model().output().clone(), edge: Edge::Left, surface: "menu-token".into(),
        });
        let reply = |request_id, body: &str| BusBridgeEvent::Reply {
            request_id, result: Ok(ctk::bus::BusReply { rc: 0, body: body.into(), result: None }),
        };
        let comp_calls = || -> Vec<_> {
            peer.drain_calls().into_iter().filter(|call| call.command.starts_with("comp")).collect()
        };
        let menu = |app: &mut App, open: bool| {
            app.world_mut().write_message(ShellCommand {
                output: test_model().output().clone(), at: Default::default(),
                kind: ShellCommandKind::Panel { edge: Edge::Left,
                    input: cosmix_shell::core::PanelInput::MenuHold(open) },
            });
        };
        let presence = |app: &mut App, services: &[&str]| {
            let live = services.iter().map(|name| (*name).to_owned()).collect();
            app.world_mut().resource_mut::<crate::holders::HolderClient>().presence(&live);
        };
        // Read, mode report, acquire: each acknowledged in turn.
        let accept_next = |app: &mut App, command: &str, body: &str| {
            app.update();
            let calls = comp_calls();
            assert_eq!(calls.len(), 1, "{command}");
            assert_eq!((calls[0].to.as_str(), calls[0].command.as_str()), ("comp-nested", command));
            peer.deliver_event(reply(calls[0].request_id, body));
            serde_json::from_str::<Value>(&calls[0].body).unwrap()
        };
        peer.deliver_event(BusBridgeEvent::Connection {
            state: BusConnectionState::Connected, generation: 1,
        });
        menu(&mut app, true);
        accept_next(&mut app, "comp.props.get", "true");
        accept_next(&mut app, "comp.panel.mode", r#"{"accepted":true}"#);
        assert_eq!(accept_next(&mut app, "comp.panel.hold", r#"{"accepted":true}"#)["acquire"], true);
        presence(&mut app, &[]);
        app.world_mut().remove_resource::<cosmix_shell_host::holders::PopupLayerIdentity>();
        menu(&mut app, false);
        app.update();
        assert!(comp_calls().is_empty(), "nothing is sent while comp is unregistered");
        presence(&mut app, &["comp-nested"]);
        accept_next(&mut app, "comp.props.get", "true");
        accept_next(&mut app, "comp.panel.mode", r#"{"accepted":true}"#);
        let release = accept_next(&mut app, "comp.panel.hold", r#"{"accepted":true}"#);
        assert_eq!(release["surface"], "menu-token");
        assert_eq!(release["acquire"], false, "the stranded hold is released");
        app.update();
        assert!(comp_calls().is_empty(), "and, acknowledged, goes quiet");
    }

    /// The model follows comp's commands exactly while the holder plane is
    /// open, and any doubt (here a gap frame) hands it back to local rules.
    #[test]
    fn comp_commands_drive_the_model_only_while_capable() {
        let (bridge, peer) = ctk::bus::test_bridge("shell");
        let mut app = bus_app(bridge);
        app.add_plugins(cosmix_shell::runtime::ShellRuntimePlugin::new(test_model()));
        let mut bus = ctk::bus::BusBridgeConfig::new("shell", "ws://127.0.0.1:9000");
        crate::holders::install(&mut app, &mut bus, "comp-nested".into());
        app.insert_resource(cosmix_shell_host::holders::PanelLayerIdentities(vec![(
            test_model().output().clone(), Edge::Left, "panel-token".into(),
        )]));
        let comp_frame = |sequence: u64, body: Value| ctk::bus::BusMessage {
            connection_generation: 1,
            from: "comp-nested".into(),
            command: "panel.command".into(),
            body: body.to_string(),
            headers: BTreeMap::from([
                ("topic".into(), "comp-nested.panel.command".into()),
                ("command".into(), "panel.command".into()),
                ("event_seq".into(), sequence.to_string()),
            ]),
        };
        let command = |sequence: u64, action: &str| comp_frame(sequence, json!({"version":1,
            "output":"test","edge":"left","surface":"panel-token","action":action,
            "event_seq":sequence}));
        let revealed = |app: &App| {
            app.world().resource::<ShellFrameState>().0.panel(Edge::Left).transient_revealed
        };
        peer.deliver_event(BusBridgeEvent::Connection {
            state: BusConnectionState::Connected, generation: 1,
        });
        app.update();
        peer.deliver_message(command(1, "reveal"));
        app.update();
        assert!(!revealed(&app), "no capability yet: comp's reveal is ignored");
        let read = peer.drain_calls().into_iter()
            .find(|call| call.command == "comp.props.get").expect("the capability read");
        peer.deliver_event(BusBridgeEvent::Reply {
            request_id: read.request_id,
            result: Ok(ctk::bus::BusReply { rc: 0, body: "true".into(), result: None }),
        });
        app.update();
        peer.deliver_message(command(2, "reveal"));
        app.update();
        assert!(revealed(&app), "a holder reveals the hidden panel");
        peer.deliver_message(command(3, "conceal"));
        app.update();
        assert!(!revealed(&app), "the last release conceals at once");
        peer.deliver_message(command(4, "reveal"));
        app.update();
        assert!(revealed(&app));
        // Lost records may include commands: back to local rules, and comp's
        // later commands are dropped until the plane is confirmed again.
        peer.deliver_message(comp_frame(5, json!({"gap":true,"lost_count":1,
            "cause":"outbox.overflow"})));
        app.update();
        peer.deliver_message(command(6, "conceal"));
        app.update();
        assert!(revealed(&app), "a local reveal is left to local grace");
    }

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
    fn panel_state_matches_props_and_host_keyboard_flags_without_mutation() {
        let mut frame = test_frame();
        for (index, panel) in frame.panels.iter_mut().enumerate() {
            panel.keyboard_focused = index % 2 == 0;
            panel.keyboard_requested = index % 2 != 0;
        }
        let before = frame.clone();
        for edge in Edge::ALL {
            for header_argument in [false, true] {
                // No provenance stamp: these reads must not require one.
                let mut req = request("shell.panel.state");
                req.body = json!({"edge":edge_name(edge)}).to_string();
                if header_argument {
                    req.body = "{}".into();
                    req.headers.insert("edge".into(), edge_name(edge).into());
                }
                let (rc, body, command) =
                    dispatch_shell_request(&req, &frame, Default::default());
                assert_eq!(rc, 0, "{body}");
                assert!(command.is_none());
                let mut actual: Value = serde_json::from_str(&body).unwrap();
                assert_eq!(actual.as_object().unwrap().len(), 10);
                assert_eq!(actual["keyboard_focused"], frame.panel(edge).keyboard_focused);
                assert_eq!(
                    actual["keyboard_requested"],
                    frame.panel(edge).keyboard_requested
                );
                actual.as_object_mut().unwrap().remove("keyboard_focused");
                actual.as_object_mut().unwrap().remove("keyboard_requested");
                let mut props = request("shell.props.get");
                props.body = json!({"path":format!("panels.{}", edge_name(edge))}).to_string();
                let (rc, body, _) =
                    dispatch_shell_request(&props, &frame, Default::default());
                assert_eq!(rc, 0);
                assert_eq!(actual, serde_json::from_str::<Value>(&body).unwrap());
            }
        }
        assert_eq!(frame, before);
    }

    #[test]
    fn panel_state_edge_errors_match_show_for_stamped_callers() {
        let frame = test_frame();
        for args in [json!({"edge":"diagonal"}), json!({}), json!({"edge":42})] {
            let mut state = request("shell.panel.state");
            state.body = args.to_string();
            let mut show = local("shell.panel.show");
            show.body = state.body.clone();
            let (rc, body, command) = dispatch_shell_request(&state, &frame, Default::default());
            let expected = dispatch_shell_request(&show, &frame, Default::default());
            assert_eq!(rc, 10);
            assert_eq!((rc, body), (expected.0, expected.1));
            assert!(command.is_none());
        }
    }

    #[test]
    fn scenes_list_empty_store_and_discovery() {
        let (bridge, peer) = test_bridge("shell");
        let mut app = bus_app(bridge);
        let mut req = request("shell.scenes.list");
        req.body = "{}".into();
        peer.send(req);
        app.update();
        let replies = peer.drain_responses();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].rc, 0);
        assert_eq!(
            serde_json::from_str::<Value>(&replies[0].body).unwrap(),
            json!([])
        );
        assert_eq!(
            app.world().resource::<ShellBusState>().diagnostics.accepted_mutations,
            0
        );
        let (rc, body, _) = dispatch_shell_request(
            &request("shell.info"),
            &test_frame(),
            Default::default(),
        );
        assert_eq!(rc, 0);
        let info: Value = serde_json::from_str(&body).unwrap();
        for verb in ["scenes.list", "panel.state"] {
            assert!(info["verbs"].as_array().unwrap().contains(&json!(verb)));
        }
    }

    #[test]
    fn scenes_list_is_sorted_reports_seats_and_matches_watch() {
        let (bridge, peer) = test_bridge("shell");
        let mut app = bus_app(bridge);
        for (name, edge) in [("zeta", "left"), ("alpha", "right")] {
            let mut load = scene_load(name, "loader", edge);
            if name == "alpha" {
                load.body = load.body.replace(
                    "\"kind\":\"edge\"",
                    "\"kind\":\"edge\",\"panel\":\"custom-id\"",
                );
            }
            peer.send(load);
            app.update();
            let replies = peer.drain_responses();
            assert_eq!(replies.len(), 1);
            assert_eq!(replies[0].rc, 0, "{}", replies[0].body);
        }
        app.world_mut()
            .resource_mut::<SubPanelRegistryState>()
            .0
            .forget("scene-zeta");
        let before = app.world().resource::<ShellFrameState>().0.clone();
        let mut req = request("shell.scenes.list");
        req.body = "{}".into();
        peer.send(req);
        app.update();
        let replies = peer.drain_responses();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].rc, 0);
        let rows: Value = serde_json::from_str(&replies[0].body).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 2);
        for (index, name, page, edge, registered) in [
            (0, "alpha", "custom-id", json!("right"), true),
            (1, "zeta", "scene-zeta", Value::Null, false),
        ] {
            let mut watch = local("shell.scene.watch");
            watch.body = json!({"scene":name}).to_string();
            peer.send(watch);
            app.update();
            let replies = peer.drain_responses();
            assert_eq!(replies.len(), 1);
            assert_eq!(replies[0].rc, 0);
            let watched: Value = serde_json::from_str(&replies[0].body).unwrap();
            assert_eq!(rows[index], json!({
                "name":name, "page":page, "edge":edge,
                "citizen":"authored-metadata", "owner":"loader", "revision":watched["revision"],
                "digest":watched["digest"], "registered":registered,
                // Unrendered in this headless fixture: nothing applied yet,
                // no diagnostics, no published model.
                "applied_revision":0, "diagnostics":[], "model_generation":null,
            }));
        }
        assert_eq!(app.world().resource::<ShellFrameState>().0, before);
        assert!(
            app.world().resource::<SubPanelRegistryState>().0.seat("scene-zeta").is_none()
        );
        assert_eq!(
            app.world()
                .resource::<cosmix_scene_bevy::SceneStore>()
                .scenes_owned_by("loader"),
            ["alpha", "zeta"]
        );
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
        for verb in ["settings.scheme", "settings.motion", "settings.size", "settings.get"] {
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

    /// Top/bottom take 24..=200 and left/right 120..=500, so the shipped
    /// 52 px bottom panel can be both stepped and restored over the verb.
    #[test]
    fn resize_range_is_per_orientation() {
        let frame = test_frame();
        let resize = |edge: &str, thickness: f64| {
            let mut req = local("shell.panel.resize");
            req.body = json!({"edge":edge, "thickness_px":thickness}).to_string();
            let (rc, body, command) =
                dispatch_shell_request(&req, &frame, std::time::Duration::ZERO);
            (rc, serde_json::from_str::<Value>(&body).unwrap(), command)
        };
        for edge in ["top", "bottom"] {
            for ok in [24.0, 52.0, 56.0, 200.0] {
                let (rc, body, command) = resize(edge, ok);
                assert_eq!(rc, 0, "{edge} {ok}: {body}");
                assert!(command.is_some());
            }
            for bad in [23.0, 201.0, 240.0] {
                let (rc, body, command) = resize(edge, bad);
                assert_eq!(rc, 10, "{edge} {bad}");
                assert!(command.is_none());
                assert_eq!(body["range_px"], json!([24.0, 200.0]));
            }
        }
        for edge in ["left", "right"] {
            let (rc, body, _) = resize(edge, 52.0);
            assert_eq!(rc, 10, "{edge} 52");
            assert_eq!(body["range_px"], json!([120.0, 500.0]));
            assert_eq!(resize(edge, 500.0).0, 0);
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

    /// The shared Bus drain updates the exact resource read by chrome.
    #[test]
    fn hotspot_bus_observations_update_chrome_resource_without_polling() {
        let (bridge, peer) = test_bridge("shell");
        let mut app = bus_app(bridge);
        let mut config = ctk::bus::BusBridgeConfig::new("shell", "ws://127.0.0.1:9000");
        crate::hotspot::install(&mut app, &mut config, "comp-nested".into());
        peer.deliver_event(BusBridgeEvent::Connection {
            state: BusConnectionState::Connected,
            generation: 1,
        });
        app.update();
        let request = peer.drain_calls().into_iter()
            .find(|call| call.command == "comp.props.get").unwrap();
        assert_eq!(request.to, "comp-nested");
        peer.deliver_event(BusBridgeEvent::Reply {
            request_id: request.request_id,
            result: Ok(ctk::bus::BusReply {
                rc: 0,
                body: json!(24.0).to_string(),
                result: None,
            }),
        });
        app.update();
        assert_eq!(app.world().resource::<cosmix_shell::chrome::QuoinHotspotSize>().0, 24.0);
        app.update();
        assert!(peer.drain_calls().is_empty());
        peer.deliver_message(BusMessage {
            connection_generation: 1,
            from: "comp-nested".into(),
            command: "props.changed".into(),
            body: json!({"path":"input.corners.deadzone_px", "new":32.0}).to_string(),
            headers: BTreeMap::from([("topic".into(), "comp-nested.props.changed".into())]),
        });
        app.update();
        let calls = peer.drain_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].command, "comp.props.get");
        assert_eq!(calls[0].to, "comp-nested");
        peer.deliver_event(BusBridgeEvent::Reply {
            request_id: calls[0].request_id,
            result: Ok(ctk::bus::BusReply {
                rc: 0,
                body: json!(32.0).to_string(),
                result: None,
            }),
        });
        app.update();
        assert_eq!(app.world().resource::<cosmix_shell::chrome::QuoinHotspotSize>().0, 32.0);
    }

    #[test]
    fn registry_gap_resync_uses_live_generation_and_events_drain_first() {
        let (bridge, peer) = test_bridge("quoin");
        let mut app = bus_app(bridge);
        let gap = |generation| BusMessage {
            connection_generation: generation,
            from: "noded".into(),
            command: "noded.topic.event".into(),
            body: "{}".into(),
            headers: BTreeMap::from([
                ("topic".into(), "noded.props.changed".into()),
                ("gap".into(), "true".into()),
            ]),
        };
        peer.deliver_event(BusBridgeEvent::Connection {
            state: BusConnectionState::Connected,
            generation: 2,
        });
        peer.deliver_message(gap(2));
        app.update();
        assert_eq!(
            peer.drain_calls()
                .iter()
                .map(|call| call.command.as_str())
                .collect::<Vec<_>>(),
            ["noded.props.get", "noded.props.get"],
            "connect then live gap; no native page telemetry requests"
        );
        peer.deliver_message(gap(1));
        app.update();
        assert!(peer.drain_calls().is_empty(), "stale gaps cannot resync");
        peer.deliver_message(gap(2));
        app.update();
        let calls = peer.drain_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].command, "noded.props.get");
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
        mounted_bus_app_with_config(None)
    }

    fn mounted_bus_app_with_config(
        config: Option<crate::config::ShellConfig>,
    ) -> (App, ctk::bus::TestBusPeer) {
        use cosmix_shell::chrome::{
            QuoinChromePlugin, QuoinContentBindings, QuoinPageRegistry, QuoinPanelMounts,
            spawn_quoin_chrome,
        };
        let (bridge, peer) = test_bridge("quoin");
        let mut app = bus_app(bridge);
        let mut model = test_model();
        let registry = if let Some(config) = config {
            let registry = QuoinPageRegistry::declared(&config.panels).unwrap();
            for edge in Edge::ALL {
                model.set_carousel(edge, registry.carousel(edge));
            }
            model.suppress_empty_edges(true);
            model.start_intro(std::time::Duration::from_secs(2));
            app.insert_resource(config);
            registry
        } else {
            QuoinPageRegistry::new(vec![], vec![], vec![], vec![]).unwrap()
        };
        app.add_plugins(cosmix_shell::runtime::ShellRuntimePlugin::new(model))
            .add_plugins(QuoinChromePlugin)
            .init_resource::<ButtonInput<KeyCode>>()
            .add_systems(
                Update,
                cosmix_scene_bevy::reconcile_scene_mounts
                    .after(ShellRuntimeSet::Input)
                    .before(ShellRuntimeSet::Model),
            );
        let world = app.world_mut();
        let props = registry
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

    #[test]
    fn empty_edges_refuse_visibility_verbs_without_enqueuing_commands() {
        let mut model = test_model();
        model.suppress_empty_edges(true);
        let frame = ShellFrame::from_model(&model);
        for edge in Edge::ALL {
            for verb in ["show", "pin", "dock", "toggle", "mode"] {
                let mut request = local(&format!("shell.panel.{verb}"));
                request.body = json!({"edge":edge_name(edge), "mode":"pinned"}).to_string();
                let (rc, body, command) =
                    dispatch_shell_request(&request, &frame, Default::default());
                assert_eq!(rc, 10, "{verb} on {edge:?}");
                let reply = serde_json::from_str::<Value>(&body).unwrap();
                assert_eq!(reply["error_code"], "EMPTY_EDGE");
                assert_eq!(reply["message"], "edge has no registered pages");
                assert!(command.is_none());
            }
        }
        for corner in ["top-left", "bottom-left", "bottom-right", "top-right"] {
            for verb in ["show", "pin", "toggle"] {
                let mut request = local(&format!("shell.corner.{verb}"));
                request.body = json!({"corner":corner}).to_string();
                let (rc, body, command) =
                    dispatch_shell_request(&request, &frame, Default::default());
                assert_eq!(rc, 10);
                let reply = serde_json::from_str::<Value>(&body).unwrap();
                assert_eq!(reply["error_code"], "EMPTY_EDGE");
                assert_eq!(reply["message"], "edge has no registered pages");
                assert!(command.is_none());
            }
        }
    }

    #[test]
    fn starts_empty_and_bottom_scene_fills_declared_slot_then_reveals() {
        for source in ["{}", r#"{panels: {bottom: ["scene-panel"]}}"#] {
            let config =
                crate::config::ShellConfig::parse(source).unwrap();
            let (mut app, peer) = mounted_bus_app_with_config(Some(config));
            let frame = &app.world().resource::<ShellFrameState>().0;
            for edge in Edge::ALL {
                assert!(frame.panel(edge).page_ids.is_empty());
                assert!(!frame.panel(edge).mapped, "intro must skip empty edges");
                assert_eq!(frame.panel(edge).exclusive_zone_px, 0.0);
            }
            load_scene(&mut app, &peer, "panel", "owner", "bottom");
            let frame = &app.world().resource::<ShellFrameState>().0;
            assert_eq!(
                frame
                    .panels
                    .iter()
                    .filter(|panel| !panel.page_ids.is_empty())
                    .count(),
                1
            );
            assert_eq!(frame.panel(Edge::Bottom).page_ids.as_ref(), ["scene-panel"]);
            assert_eq!(
                frame.panel(Edge::Bottom).active_page_id.as_deref(),
                Some("scene-panel")
            );
            let mut show = local("shell.panel.show");
            show.body = json!({"edge":"bottom"}).to_string();
            peer.send(show);
            app.update();
            assert_eq!(peer.drain_responses()[0].rc, 0);
            let frame = &app.world().resource::<ShellFrameState>().0;
            assert!(frame.panel(Edge::Bottom).transient_revealed);
            assert_eq!(
                frame.panel(Edge::Bottom).active_page_id.as_deref(),
                Some("scene-panel")
            );
        }
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

    #[test]
    fn panel_notifications_report_applied_pages_and_suppress_idle_duplicates() {
        let (mut app, peer) = mounted_bus_app();
        peer.drain_publishes(); // Discard the initial empty applied snapshot.
        peer.send(scene_load("event-page", "scenes", "right"));
        app.update();
        let notices = peer.drain_publishes();
        let notice = notices.iter().find(|p| p.headers.get("name").is_some_and(|n| n == "quoin.panel.changed")).unwrap();
        let (_, body) = notice.body.split_once("\n---\n").unwrap();
        let body: Value = serde_json::from_str(body).unwrap();
        assert!(body["panels"]["right"]["pages"].as_array().unwrap().contains(&json!("scene-event-page")));
        let revision = body["revision"].as_u64().unwrap();
        app.update();
        assert!(peer.drain_publishes().iter().all(|p| p.headers.get("name").is_none_or(|n| n != "quoin.panel.changed")));
        peer.deliver_event(BusBridgeEvent::Connection {state:BusConnectionState::Connected, generation:2});
        app.update();
        let notices = peer.drain_publishes();
        let notice = notices.iter().find(|p| p.headers.get("name").is_some_and(|n| n == "quoin.panel.changed")).unwrap();
        let (_, body) = notice.body.split_once("\n---\n").unwrap();
        let body: Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["generation"], 2);
        assert!(body["revision"].as_u64().unwrap() > revision);
    }

    #[test]
    fn panel_notices_coalesce_resize_and_reveal_frames() {
        let mut frame = test_frame();
        let before = panel_notice_snapshot(&frame, &NO_DECLARED);
        for fraction in [0.1, 0.25, 0.5, 0.75, 1.0] {
            let panel = &mut frame.panels[Edge::Left.index()];
            panel.resize_active = true;
            panel.thickness_px += 1.0;
            panel.visible_fraction = fraction;
            assert_eq!(panel_notice_snapshot(&frame, &NO_DECLARED), before);
        }
        let panel = &mut frame.panels[Edge::Left.index()];
        panel.resize_active = false;
        panel.settled_thickness_px = panel.thickness_px;
        let settled = panel_notice_snapshot(&frame, &NO_DECLARED);
        assert_ne!(settled, before);
        assert_eq!(settled["panels"]["left"]["width_px"], json!(frame.panel(Edge::Left).thickness_px));
        frame.panels[Edge::Left.index()].mapped = !frame.panel(Edge::Left).mapped;
        assert_ne!(panel_notice_snapshot(&frame, &NO_DECLARED), settled);
    }

    /// The frozen request/reply/refusal shapes of
    /// `fixtures/scene-editor/shell-verbs.json`.
    #[test]
    fn panel_order_matches_the_frozen_fixture() {
        let fixtures: Value = serde_json::from_str(include_str!(
            "../../../scripts/tests/fixtures/scene-editor/shell-verbs.json"
        ))
        .unwrap();
        let fixture = &fixtures["shell.panel.order"];
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("conf.mix");
        std::fs::write(&path, r#"{panels: {right: ["scene-notes"], left: ["scene-calendar"]}}"#)
            .unwrap();
        let (rc, reply) = panel_order(&fixture["request"].to_string(), &path);
        assert_eq!(rc, 0, "{reply}");
        assert_eq!(reply, fixture["reply"]);
        let written =
            crate::config::ShellConfig::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            written.panels[Edge::Right.index()],
            ["scene-notes", "settings.appearance"]
        );

        let expected = &fixture["refusals"]["INVALID_ARGUMENT"];
        let (rc, reply) = panel_order(
            &json!({"edges": {"left": ["scene-notes"], "right": ["scene-notes"]}}).to_string(),
            &path,
        );
        assert_eq!(rc, 10);
        assert_eq!(reply, *expected);
        let (rc, reply) = panel_order("not json", &path);
        assert_eq!((rc, reply["error_code"].as_str()), (10, Some("INVALID_ARGUMENT")));

        // An unwritable location is CONFIG_WRITE, never a partial file.
        let blocked = directory.path().join("file-not-dir");
        std::fs::write(&blocked, "").unwrap();
        let (rc, reply) = panel_order(
            &fixture["request"].to_string(),
            &blocked.join("conf.mix"),
        );
        assert_eq!(rc, 10);
        assert_eq!(reply["error_code"], fixture["refusals"]["CONFIG_WRITE"]["error_code"]);
    }

    fn panel_notices(peer: &ctk::bus::TestBusPeer) -> Vec<Value> {
        peer.drain_publishes()
            .iter()
            .filter(|p| p.headers.get("name").is_some_and(|n| n == "quoin.panel.changed"))
            .map(|p| serde_json::from_str(p.body.split_once("\n---\n").unwrap().1).unwrap())
            .collect()
    }

    /// Scene-editor plan §4.3 Q2 (round-2 Opus N7): every `dialog.visible`
    /// or `dialog.scene` change publishes `panel.changed` with a strictly
    /// greater revision even when no panel changed, so the loader's revision
    /// filter never drops a hide; `props.get` reports the same seat.
    #[test]
    fn a_dialog_only_change_publishes_a_new_revision_and_reaches_props() {
        let (mut app, peer) = mounted_bus_app();
        crate::dialog_bus::install(&mut app);
        app.update();
        panel_notices(&peer);
        let source = "---\nscene: 1\nname: editor\ncitizen: scene-editor\nwindow: {\"h\":620,\"kind\":\"dialog\",\"title\":\"Scene Editor\",\"w\":880}\n---\n```mix\nroot: {widget: \"column\", children: []}\n```\n";
        let mut load = local("shell.scene.load");
        load.from = "scenes".into();
        load.body = json!({"source": source, "model_generation": 1}).to_string();
        peer.send(load);
        app.update();
        let loaded = panel_notices(&peer).pop().expect("loading the dialog publishes");
        assert_eq!(loaded["dialog"]["scene"], "editor");
        assert_eq!(loaded["dialog"]["visible"], false);
        let panels = loaded["panels"].clone();
        let mut revision = loaded["revision"].as_u64().unwrap();
        for (verb, visible) in [
            ("shell.dialog.show", true),
            ("shell.dialog.hide", false),
            ("shell.dialog.show", true),
        ] {
            let mut request = local(verb);
            request.body = json!({"scene":"editor"}).to_string();
            peer.send(request);
            app.update();
            let notices = panel_notices(&peer);
            assert_eq!(notices.len(), 1, "{verb}: exactly one notice");
            assert_eq!(notices[0]["dialog"]["visible"], visible, "{verb}");
            assert_eq!(notices[0]["panels"], panels, "{verb}: a dialog-only change");
            let next = notices[0]["revision"].as_u64().unwrap();
            assert!(next > revision, "{verb}: revision {next} after {revision}");
            revision = next;
        }
        app.update();
        assert!(panel_notices(&peer).is_empty(), "an unchanged snapshot publishes nothing");
        let mut props = local("shell.props.get");
        props.body = json!({"path": "dialog"}).to_string();
        peer.send(props);
        app.update();
        let replies = peer.drain_responses();
        let reply = replies
            .iter()
            .find(|reply| reply.command == "shell.props.get")
            .expect("props.get answered");
        let body: Value = serde_json::from_str(&reply.body).unwrap();
        assert_eq!(
            body,
            json!({"scene":"editor","visible":true,"w":880.0,"h":620.0,"output":"test"}),
            "{body}"
        );
    }

    /// Scene-editor plan §4.3 Q1: `panel.changed` and `props.get` carry the
    /// dialog seat (null before Q2) and each panel's `conf.mix` order; a new
    /// declared order publishes once with a new revision, an unchanged
    /// snapshot publishes nothing.
    #[test]
    fn notices_and_props_carry_declared_order_and_a_null_dialog() {
        let config = crate::config::ShellConfig::parse(
            r#"{panels: {right: ["scene-notes", "settings.appearance"]}}"#,
        )
        .unwrap();
        let (mut app, peer) = mounted_bus_app_with_config(Some(config));
        // The mount helper's drain discarded the first notice; a reconnect
        // republishes the full snapshot.
        peer.deliver_event(BusBridgeEvent::Connection {
            state: BusConnectionState::Connected,
            generation: 2,
        });
        app.update();
        let notices = panel_notices(&peer);
        let first = notices.last().expect("the connected host publishes a snapshot");
        assert_eq!(first["dialog"], Value::Null);
        assert!(first.as_object().unwrap().contains_key("dialog"));
        assert_eq!(
            first["panels"]["right"]["declared"],
            json!(["scene-notes", "settings.appearance"])
        );
        for edge in ["left", "bottom", "top"] {
            assert_eq!(first["panels"][edge]["declared"], json!([]), "{edge}");
        }
        let revision = first["revision"].as_u64().unwrap();
        app.update();
        assert!(panel_notices(&peer).is_empty(), "unchanged: nothing published");

        let reordered = crate::config::ShellConfig::parse(
            r#"{panels: {right: ["settings.appearance", "scene-notes"]}}"#,
        )
        .unwrap();
        app.insert_resource(reordered.clone());
        app.update();
        let notices = panel_notices(&peer);
        assert_eq!(notices.len(), 1);
        assert!(notices[0]["revision"].as_u64().unwrap() > revision);
        assert_eq!(
            notices[0]["panels"]["right"]["declared"],
            json!(["settings.appearance", "scene-notes"])
        );
        app.update();
        assert!(panel_notices(&peer).is_empty());

        // props.get reports the same fields.
        let frame = app.world().resource::<ShellFrameState>().0.clone();
        let get = |path: &str| {
            let mut props = request("shell.props.get");
            props.body = json!({"path": path}).to_string();
            let (rc, body, _) =
                dispatch_with_declared(&props, &frame, &reordered.panels, &PropValue::Null, Default::default());
            assert_eq!(rc, 0, "{path}: {body}");
            serde_json::from_str::<Value>(&body).unwrap()
        };
        assert_eq!(get("dialog"), Value::Null);
        assert_eq!(
            get("panels.right.declared"),
            json!(["settings.appearance", "scene-notes"])
        );
        assert_eq!(get("panels.top.declared"), json!([]));
    }

    #[test]
    fn panel_replies_confirm_application_without_notices() {
        let (mut app, peer) = mounted_bus_app();
        app.insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_millis(16),
        ));
        load_scene(&mut app, &peer, "reply-page", "owner", "left");
        peer.drain_responses();
        for (verb, args) in [
            ("shell.panel.page.set", json!({"edge":"left", "id":"scene-reply-page"})),
            ("shell.panel.pin", json!({"edge":"left"})),
            ("shell.panel.mode", json!({"edge":"left", "mode":"hidden"})),
        ] {
            let mut req = local(verb);
            req.body = args.to_string();
            peer.send(req);
            assert!(peer.drain_responses().is_empty());
            let mut replies = Vec::new();
            // Drive the model's native animation frames, discarding every
            // notice. There is no client-side notification continuation.
            // Both drains read one channel and discard the other kind, so
            // take responses first; the remaining notices are then dropped.
            for _ in 0..120 {
                app.update();
                replies.extend(peer.drain_responses());
                peer.drain_publishes();
                if !replies.is_empty() { break; }
            }
            assert_eq!(replies.len(), 1, "{verb}");
            assert_eq!(replies[0].rc, 0, "{}", replies[0].body);
            let body: Value = serde_json::from_str(&replies[0].body).unwrap();
            assert_eq!(body["applied"], true);
            let snapshot = Value::from(&ShellProps(&app.world().resource::<ShellFrameState>().0, &NO_DECLARED, PropValue::Null).snapshot());
            assert_eq!(body["panels"], snapshot["panels"]);
            if verb == "shell.panel.pin" {
                // Let reveal progress before hiding; otherwise concealment
                // can finish immediately from a still-zero motion fraction.
                for _ in 0..30 {
                    app.update();
                    peer.drain_publishes();
                }
            }
            if verb == "shell.panel.mode" {
                assert_eq!(body["panels"]["left"]["pinned"], false);
                assert_eq!(body["panels"]["left"]["visible"], false);
            }
        }
    }

    #[test]
    fn superseded_panel_command_is_refused_with_applied_state() {
        let (mut app, peer) = mounted_bus_app();
        let mut pin = local("shell.panel.pin");
        pin.body = json!({"edge":"left"}).to_string();
        let mut hide = local("shell.panel.mode");
        hide.body = json!({"edge":"left", "mode":"hidden"}).to_string();
        peer.send(pin);
        peer.send(hide);
        app.update();
        let replies = peer.drain_responses();
        let pin = replies.iter().find(|reply| reply.command == "shell.panel.pin").unwrap();
        assert_eq!(pin.rc, 10);
        let body: Value = serde_json::from_str(&pin.body).unwrap();
        assert_eq!(body["error_code"], "PANEL_NOT_APPLIED");
        assert_eq!(body["panels"]["left"]["pinned"], false);
    }

    #[test]
    fn hide_reply_applies_mode_when_pointer_reveals_during_concealment() {
        let (mut app, peer) = mounted_bus_app();
        app.insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_millis(16),
        ));
        load_scene(&mut app, &peer, "held-reply", "owner", "left");
        let mut pin = local("shell.panel.pin");
        pin.body = json!({"edge":"left"}).to_string();
        peer.send(pin);
        for _ in 0..30 { app.update(); }
        peer.drain_responses();
        let mut hide = local("shell.panel.mode");
        hide.body = json!({"edge":"left", "mode":"hidden"}).to_string();
        peer.send(hide);
        app.update();
        assert!(peer.drain_responses().is_empty(), "concealment holds the reply");
        app.world_mut().write_message(ShellCommand {
            output: test_model().output().clone(),
            at: std::time::Duration::from_secs(1),
            kind: ShellCommandKind::Panel { edge: Edge::Left, input: PanelInput::CornerEntered },
        });
        app.update();
        let replies = peer.drain_responses();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].rc, 0, "{}", replies[0].body);
        let body: Value = serde_json::from_str(&replies[0].body).unwrap();
        assert_eq!(body["applied"], true);
        assert_eq!(body["panels"]["left"]["mode"], "hidden");
        assert_eq!(body["panels"]["left"]["visible"], true);
    }

    #[test]
    fn behaviour_disconnect_does_not_remove_loader_owned_scene() {
        let (mut app, peer) = mounted_bus_app();
        load_scene(&mut app, &peer, "frozen", "scenes", "right");
        peer.deliver_message(services_registered_change(1,
            &["scenes", "authored-metadata"], &["scenes"]));
        app.update();
        assert_eq!(app.world().resource::<cosmix_scene_bevy::SceneStore>().scenes_owned_by("scenes"), ["frozen"]);
        assert!(app.world().resource::<ShellFrameState>().0.panel(Edge::Right).page_ids.iter().any(|id| id == "scene-frozen"));
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

    /// Answer the snapshot a departure message now requests: `keeper` and the
    /// shell are registered, everyone else is gone.
    /// A full outbound queue defers the request to the retry on the next
    /// update, so look again after one.
    fn confirm_absent(app: &mut App, peer: &ctk::bus::TestBusPeer) {
        let mut id = None;
        for _ in 0..3 {
            id = peer
                .drain_calls()
                .into_iter()
                .find(|call| call.command == "noded.props.get")
                .map(|call| call.request_id);
            if id.is_some() {
                break;
            }
            app.update();
        }
        let id = id.expect("a departure must request a confirming snapshot");
        reply_citizen_snapshot(peer, id, &["shell", "keeper"]);
        app.update();
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
        confirm_absent(&mut app, &peer);
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
        // The test peer drains one shared queue, so take the confirming
        // snapshot request before anything else reads it; the replacement
        // load is judged by the registry below.
        let id = citizen_snapshot_id(&peer);
        reply_citizen_snapshot(&peer, id, &["shell", "keeper"]);
        app.update();
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

    /// The departure message drains in a LATER frame than the restarted
    /// owner's fresh load (the topic and the request channel are unordered).
    /// It must not drop the fresh load: the departure is confirmed by a
    /// request-time-fenced snapshot, which finds the owner back.
    #[test]
    fn citizen_departure_draining_after_a_replacement_load_keeps_it() {
        let (mut app, peer) = mounted_bus_app();
        load_scene(&mut app, &peer, "notes", "owner", "left");
        // The owner restarts: its fresh load lands first, in its own frame.
        load_scene(&mut app, &peer, "notes", "owner", "left");
        peer.drain_publishes();
        // Only now does the old process's departure drain.
        absent(&peer);
        app.update();
        assert!(
            app.world().resource::<SubPanelRegistryState>().0.seat("scene-notes").is_some(),
            "a topic departure alone never removes content"
        );
        let id = citizen_snapshot_id(&peer);
        reply_citizen_snapshot(&peer, id, &["shell", "keeper", "owner"]);
        app.update();
        assert!(app.world().resource::<SubPanelRegistryState>().0.seat("scene-notes").is_some());
        assert_eq!(
            app.world().resource::<cosmix_scene_bevy::SceneStore>().scenes_owned_by("owner"),
            ["notes"]
        );
        assert!(
            app.world().resource::<ShellFrameState>().0.panel(Edge::Left).page_ids.iter().any(|id| id == "scene-notes"),
            "the panel is still on screen"
        );
    }

    /// A confirmed departure unloads and says so: `shell.scene.changed`
    /// `{ops:["unloaded"], reason:"owner_departed"}` for each scene.
    #[test]
    fn citizen_departure_unload_publishes_a_notice() {
        let (mut app, peer) = mounted_bus_app();
        load_scene(&mut app, &peer, "notes", "owner", "left");
        peer.drain_publishes();
        absent(&peer);
        app.update();
        confirm_absent(&mut app, &peer);
        let notices: Vec<Value> = peer
            .drain_publishes()
            .iter()
            .filter(|p| p.headers.get("name").is_some_and(|n| n.ends_with(".scene.changed")))
            .map(|p| serde_json::from_str(p.body.split_once("\n---\n").unwrap().1).unwrap())
            .collect();
        let departed: Vec<&Value> = notices.iter().filter(|n| n["reason"] == "owner_departed").collect();
        assert_eq!(departed.len(), 1, "{notices:?}");
        assert_eq!(departed[0]["scene"], "notes");
        assert_eq!(departed[0]["ops"], json!(["unloaded"]));
        assert_eq!(departed[0]["owner"], "owner");
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
        confirm_absent(&mut app, &peer);
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
        // Mesh and anonymous owners are not tracked lifetimes: no departure,
        // so no confirming snapshot is even requested.
        assert!(peer.drain_calls().iter().all(|call| call.command != "noded.props.get"));
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

    /// `shell.settings.changed` notices published since the last drain.
    fn settings_notices(peer: &ctk::bus::TestBusPeer) -> Vec<Value> {
        peer.drain_publishes()
            .iter()
            .filter(|p| p.headers.get("name").is_some_and(|n| n == "quoin.settings.changed"))
            .map(|p| {
                let (head, body) = p.body.split_once("\n---\n").unwrap();
                assert!(head.contains("command: shell.settings.changed"), "{head}");
                serde_json::from_str(body).unwrap()
            })
            .collect()
    }

    /// The settings template's behaviour builds its model from
    /// `shell.settings.get` and wakes on `shell.settings.changed`: the read
    /// carries every choice, a scheme selected over the Bus OR from chrome
    /// publishes exactly one notice with the new state, an idle update
    /// publishes nothing, and a reconnect republishes under the new epoch.
    #[test]
    fn settings_get_snapshot_and_change_notices() {
        // The test peer's calls, responses and publishes share one queue, and
        // each drain discards the other kinds: drain one kind per step. The
        // fixture's connect update published revision 1 (drained with calls).
        let (mut app, peer) = sub_panel_app();
        let (rc, got) = sub_send(&mut app, &peer, "shell.settings.get", json!({}));
        assert_eq!(rc, 0, "{got}");
        assert_eq!(got["revision"], 1, "the connected host published its first snapshot");
        assert_eq!(got["scheme"], "ocean");
        assert_eq!(got["motion"], "slide");
        assert_eq!(got["schemes"].as_array().unwrap().len(), 6);
        assert_eq!(got["fade_reason"], crate::settings::FADE_UNAVAILABLE_REASON);
        assert_eq!(got["motions"][1]["available"], false);
        assert_eq!(got["generation"], 1);
        assert_eq!(got["page_owner"], Value::Null);
        for edge in Edge::ALL {
            let settled = app.world().resource::<ShellFrameState>().0.panel(edge).settled_thickness_px;
            assert_eq!(got["sizes"][edge_name(edge)], json!(settled));
        }
        app.update();
        assert!(settings_notices(&peer).is_empty(), "a read publishes nothing");

        // The write's reply is covered by the settings tests; here its notice.
        peer.send(wire("shell.settings.scheme", json!({"name":"forest"})));
        app.update();
        let notices = settings_notices(&peer);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0]["scheme"], "forest");
        assert_eq!(notices[0]["generation"], 1);
        let revision = notices[0]["revision"].as_u64().unwrap();
        assert_eq!(revision, 2);
        let (_, got) = sub_send(&mut app, &peer, "shell.settings.get", json!({}));
        assert_eq!((got["scheme"].as_str(), got["revision"].as_u64()), (Some("forest"), Some(revision)));
        app.update();
        assert!(settings_notices(&peer).is_empty(), "no idle publication");

        // A chrome scheme dot writes the same message; the notice follows it.
        app.world_mut()
            .write_message(cosmix_shell::chrome::QuoinSchemeSelected(ctk::theme::Scheme::Mono));
        app.update();
        let notices = settings_notices(&peer);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0]["scheme"], "mono");

        peer.deliver_event(BusBridgeEvent::Connection {
            state: BusConnectionState::Connected,
            generation: 2,
        });
        app.update();
        let notices = settings_notices(&peer);
        assert_eq!(notices.len(), 1, "a reconnect republishes the unchanged snapshot");
        assert_eq!(notices[0]["generation"], 2);
        assert_eq!(notices[0]["scheme"], "mono");
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

    /// Chunk 16: a minimal comp for [`pump`] — it answers reads and verbs,
    /// and for the left edge keeps comp's holder verdict (explicit holds
    /// plus its own keyboard-focus membership), publishing `panel.command`
    /// on a change and re-stating it for every hidden mode report.
    struct FakeComp {
        capable: std::cell::Cell<bool>,
        focus: std::cell::RefCell<Value>,
        outputs: std::cell::RefCell<BTreeMap<u64, &'static str>>,
        /// Refuse hold acquisitions with this code — `unknown_panel_surface`
        /// for a layer comp has not mapped yet, `locked` under a session
        /// lock (comp refuses before touching holder state, publishing
        /// nothing).
        refuse_holds: std::cell::Cell<Option<&'static str>>,
        left_surface: std::cell::RefCell<Option<String>>,
        left_hidden: std::cell::Cell<bool>,
        left_holds: std::cell::RefCell<BTreeSet<String>>,
        left_focused: std::cell::Cell<bool>,
        verdict: std::cell::Cell<Option<bool>>,
        sequence: std::cell::Cell<u64>,
        /// Every command published, in order: `true` reveal, `false` conceal.
        published: std::cell::RefCell<Vec<bool>>,
        dirty: std::cell::Cell<bool>,
    }

    impl FakeComp {
        fn new(capable: bool) -> Self {
            Self {
                capable: capable.into(),
                focus: json!({"keyboard":null,"pointer":null}).into(),
                outputs: BTreeMap::new().into(),
                refuse_holds: None.into(),
                left_surface: None.into(),
                left_hidden: true.into(),
                left_holds: BTreeSet::new().into(),
                left_focused: false.into(),
                verdict: None.into(),
                sequence: 0.into(),
                published: Vec::new().into(),
                dirty: false.into(),
            }
        }

        fn settle(&self, peer: &ctk::bus::TestBusPeer, restate: bool) {
            let Some(surface) = self.left_surface.borrow().clone() else { return };
            if !self.left_hidden.get() {
                self.verdict.set(None);
                return;
            }
            let holding = self.left_focused.get() || !self.left_holds.borrow().is_empty();
            if restate || self.verdict.get() != Some(holding) {
                self.verdict.set(Some(holding));
                self.sequence.set(self.sequence.get() + 1);
                self.published.borrow_mut().push(holding);
                self.dirty.set(true);
                peer.deliver_message(panel_command(self.sequence.get(), &surface,
                    if holding { "reveal" } else { "conceal" }));
            }
        }

        /// Keyboard focus moves onto (or off) the left panel's layer.
        fn set_focused(&self, peer: &ctk::bus::TestBusPeer, focused: bool) {
            self.left_focused.set(focused);
            self.settle(peer, false);
        }

        fn published(&self) -> Vec<bool> {
            std::mem::take(&mut *self.published.borrow_mut())
        }
    }

    /// Run updates, answering every call to comp as comp would, until an
    /// update sends none and comp published nothing; returns those calls.
    /// Every comp verb Quoin sends must be the literal `comp.*` command
    /// addressed to the instance.
    fn pump(app: &mut App, peer: &ctk::bus::TestBusPeer, comp: &FakeComp) -> Vec<ctk::bus::TestBusCall> {
        let mut seen = Vec::new();
        for _ in 0..32 {
            app.update();
            let calls: Vec<_> = peer.drain_calls().into_iter()
                .filter(|call| call.to == "comp-nested").collect();
            if calls.is_empty() && !comp.dirty.replace(false) {
                return seen;
            }
            for call in calls {
                assert!(call.command.starts_with("comp."), "literal comp verb: {}", call.command);
                let body: Value = serde_json::from_str(&call.body).unwrap();
                let left = body["edge"] == "left";
                let (rc, reply) = match (call.command.as_str(), body["path"].as_str()) {
                    ("comp.props.get", Some("input.corners.holders")) => (0, json!(comp.capable.get())),
                    ("comp.props.get", Some("focus")) => (0, comp.focus.borrow().clone()),
                    ("comp.props.get", Some(path)) => {
                        let id = path.strip_prefix("surfaces.s")
                            .and_then(|rest| rest.strip_suffix(".output"))
                            .and_then(|id| id.parse::<u64>().ok())
                            .unwrap_or_else(|| panic!("unexpected read {path}"));
                        (0, json!(comp.outputs.borrow().get(&id)))
                    }
                    ("comp.panel.hold", _) if body["acquire"] == true && comp.refuse_holds.get().is_some() => {
                        (10, json!({"error":comp.refuse_holds.get(),"surface":body["surface"]}))
                    }
                    _ => (0, json!({"accepted":true,"surface":body["surface"]})),
                };
                peer.deliver_event(BusBridgeEvent::Reply {
                    request_id: call.request_id,
                    result: Ok(ctk::bus::BusReply { rc, body: reply.to_string(), result: None }),
                });
                match call.command.as_str() {
                    "comp.panel.mode" if left => {
                        *comp.left_surface.borrow_mut() = body["surface"].as_str().map(str::to_owned);
                        comp.left_hidden.set(body["mode"] == "hidden");
                        if body["mode"] != "hidden" {
                            comp.left_holds.borrow_mut().clear();
                        }
                        comp.settle(peer, true);
                    }
                    "comp.panel.hold" if left && rc == 0 && comp.left_hidden.get() => {
                        let holder = body["holder"].as_str().unwrap().to_owned();
                        if body["acquire"] == true {
                            comp.left_holds.borrow_mut().insert(holder);
                        } else {
                            comp.left_holds.borrow_mut().remove(&holder);
                        }
                        comp.settle(peer, false);
                    }
                    _ => {}
                }
                seen.push(call);
            }
        }
        panic!("comp traffic never settled");
    }

    fn comp_message(suffix: &str, body: Value) -> BusMessage {
        BusMessage {
            connection_generation: 1,
            from: "comp-nested".into(),
            command: suffix.into(),
            body: body.to_string(),
            headers: BTreeMap::from([
                ("topic".into(), format!("comp-nested.{suffix}")),
                ("command".into(), suffix.into()),
            ]),
        }
    }

    fn focus_changed(keyboard: Option<u64>, previous: Option<u64>) -> BusMessage {
        comp_message("focus.changed", json!({"keyboard":keyboard,"previous":previous,
            "exclusive_latch":null,"event_seq":1}))
    }

    fn holds(calls: &[ctk::bus::TestBusCall]) -> Vec<Value> {
        calls.iter().filter(|call| call.command == "comp.panel.hold")
            .map(|call| serde_json::from_str(&call.body).unwrap()).collect()
    }

    /// A connected Quoin with the standalone holder client and activation
    /// targeting against `comp-nested`, and `alpha` + `beta` registered on
    /// the left edge. Left has no layer token yet: a hidden panel has no
    /// layer until a reveal maps one (the test inserts it then); the other
    /// edges report their modes during setup.
    ///
    /// Harness limit: `TestBusPeer::drain_calls` and `drain_responses` share
    /// one queue and each discards what the other would return, so a verb
    /// sent with [`sub_send`] must not also send a comp call in the same
    /// update. The tests arrange that the way the live host does — the left
    /// layer token appears only after the reveal — and drive mode, focus and
    /// Escape with `write_message` rather than over the Bus.
    fn activation_app(comp: &FakeComp) -> (App, ctk::bus::TestBusPeer) {
        let (bridge, peer) = test_bridge("quoin");
        let mut app = bus_app(bridge);
        app.add_plugins(cosmix_shell::runtime::ShellRuntimePlugin::new(test_model()));
        let mut bus = ctk::bus::BusBridgeConfig::new("quoin", "ws://127.0.0.1:9000");
        crate::activation::install(&mut app, &mut bus, "comp-nested".into());
        crate::holders::install(&mut app, &mut bus, "comp-nested".into());
        assert_eq!(
            bus.subscriptions.iter().filter(|topic| *topic == "comp-nested.focus.changed").count(),
            1,
            "one subscription to comp's focus topic"
        );
        app.insert_resource(cosmix_shell_host::holders::PanelLayerIdentities(
            [Edge::Top, Edge::Right, Edge::Bottom].into_iter()
                .map(|edge| (test_model().output().clone(), edge, format!("panel-{}", edge_name(edge))))
                .collect(),
        ));
        peer.deliver_event(BusBridgeEvent::Connection {
            state: BusConnectionState::Connected,
            generation: 1,
        });
        pump(&mut app, &peer, comp);
        peer.drain_responses();
        for name in ["alpha", "beta"] {
            let (rc, body) = sub_send(&mut app, &peer, "shell.sub.register",
                json!({"edge":"left","name":name}));
            assert_eq!(rc, 0, "{body}");
        }
        (app, peer)
    }

    /// Host input for the model: which panel surface holds the keyboard,
    /// or an Escape reaching the focused panel.
    fn keyboard(app: &mut App, command: cosmix_shell::runtime::KeyboardCommand) {
        app.world_mut().write_message(ShellCommand {
            output: test_model().output().clone(),
            at: Default::default(),
            kind: ShellCommandKind::Keyboard(command),
        });
    }

    fn observe_focus(app: &mut App, edge: Option<Edge>) {
        keyboard(app, cosmix_shell::runtime::KeyboardCommand::FocusObserved(edge));
    }

    fn left(app: &App) -> cosmix_shell::runtime::PanelPresentation {
        app.world().resource::<ShellFrameState>().0.panel(Edge::Left).clone()
    }

    fn map_left_layer(app: &mut App, token: &str) {
        let mut identities = app.world_mut()
            .resource_mut::<cosmix_shell_host::holders::PanelLayerIdentities>();
        identities.0.retain(|(_, edge, _)| *edge != Edge::Left);
        identities.0.push((test_model().output().clone(), Edge::Left, token.into()));
    }

    fn unmap_left_layer(app: &mut App) {
        app.world_mut()
            .resource_mut::<cosmix_shell_host::holders::PanelLayerIdentities>()
            .0
            .retain(|(_, edge, _)| *edge != Edge::Left);
    }

    fn panel_command(event_seq: u64, surface: &str, action: &str) -> BusMessage {
        comp_message("panel.command", json!({"version":1,"output":"test","edge":"left",
            "surface":surface,"action":action,"event_seq":event_seq}))
    }

    /// Activate `beta` on the hidden left edge, map its layer and let comp
    /// acknowledge the focus hold: the state every hidden-edge test starts
    /// from. Returns the hold comp received.
    fn activate_hidden_and_hold(app: &mut App, peer: &ctk::bus::TestBusPeer, comp: &FakeComp,
        token: &str) -> Value {
        let (rc, body) = sub_send(app, peer, "shell.sub.activate", json!({"name":"beta"}));
        assert_eq!(rc, 0, "{body}");
        map_left_layer(app, token);
        let acquired = holds(&pump(app, peer, comp));
        assert_eq!(acquired.len(), 1, "{acquired:?}");
        assert!(left(app).transient_revealed);
        acquired[0].clone()
    }

    #[test]
    fn activate_unknown_subpanel_is_refused() {
        let comp = FakeComp::new(true);
        let (mut app, peer) = activation_app(&comp);
        let before = left(&app);
        for (body, fragment) in [
            (json!({"name":"ghost"}), "'ghost' is not registered"),
            (json!({}), "requires a name"),
            (json!({"name":"  "}), "requires a name"),
            (json!({"name":"alpha","focus":"maybe"}), "focus must be true or false"),
            (json!({"name":"alpha","focus":1}), "focus must be true or false"),
        ] {
            let (rc, reply) = sub_send(&mut app, &peer, "shell.sub.activate", body.clone());
            assert_eq!(rc, 10, "{body}: {reply}");
            assert!(reply["error"].as_str().unwrap().contains(fragment), "{body}: {reply}");
            assert!(reply.get("error_code").is_none(), "not a capability refusal: {reply}");
        }
        // The same refusal `sub.remove` gives: activation never creates.
        let (_, removal) = sub_send(&mut app, &peer, "shell.sub.remove", json!({"name":"ghost"}));
        let (_, activation) = sub_send(&mut app, &peer, "shell.sub.activate", json!({"name":"ghost"}));
        assert_eq!(activation, removal);
        // Provenance first, before any argument is read.
        let mut unattested = request("shell.sub.activate");
        unattested.body = json!({"name":"alpha"}).to_string();
        peer.send(unattested);
        app.update();
        let replies = peer.drain_responses();
        assert_eq!(replies[0].rc, 10);
        assert!(replies[0].body.contains("sub-panel caller provenance"));
        assert_eq!(left(&app), before, "no refusal touches the panel");
        assert!(app.world().resource::<SubPanelRegistryState>().0.seat("ghost").is_none());
    }

    /// The B→C window: comp without the holder plane (or no holder client
    /// at all, as in the embedded host) refuses with the reason — never a
    /// silent no-op, never a local reveal.
    #[test]
    fn activate_while_uncapable_is_refused_with_reason() {
        let comp = FakeComp::new(false);
        let (mut app, peer) = activation_app(&comp);
        let before = left(&app);
        assert_eq!(before.active_page_id.as_deref(), Some("alpha"));
        for remove_client in [false, true] {
            if remove_client {
                app.world_mut().remove_resource::<crate::holders::HolderClient>();
            }
            for focus in [true, false] {
                let (rc, body) = sub_send(&mut app, &peer, "shell.sub.activate",
                    json!({"name":"beta","focus":focus}));
                assert_eq!(rc, 10, "{body}");
                assert_eq!(body["error_code"], "ACTIVATION_UNAVAILABLE");
                assert_eq!(body["reason"], "compositor holder plane not available");
                assert_eq!(body["name"], "beta");
                assert!(body["error"].as_str().unwrap().contains("compositor holder plane not available"));
                assert_eq!(left(&app), before, "no reveal, page switch or focus request");
                assert!(holds(&pump(&mut app, &peer, &comp)).is_empty());
            }
        }
        // An unknown name is still the unknown-name refusal while uncapable.
        let (_, body) = sub_send(&mut app, &peer, "shell.sub.activate", json!({"name":"ghost"}));
        assert!(body["error"].as_str().unwrap().contains("not registered"), "{body}");
        // Discoverable: the verb is advertised, refusal and all.
        let (_, info, _) = dispatch_shell_request(&request("shell.info"), &test_frame(), Default::default());
        let info: Value = serde_json::from_str(&info).unwrap();
        assert!(info["verbs"].as_array().unwrap().contains(&json!("sub.activate")));
    }

    /// End to end across the B→C window: refused while comp reports no
    /// holder plane; comp's leaf changes (restart C), Quoin re-reads it on
    /// the `props.changed`, and the same request is accepted and held.
    #[test]
    fn activation_is_refused_until_the_holder_plane_arrives() {
        let comp = FakeComp::new(false);
        let (mut app, peer) = activation_app(&comp);
        let (rc, body) = sub_send(&mut app, &peer, "shell.sub.activate", json!({"name":"beta"}));
        assert_eq!((rc, &body["error_code"]), (10, &json!("ACTIVATION_UNAVAILABLE")));
        comp.capable.set(true);
        peer.deliver_message(comp_message("props.changed", json!({"path":"input.corners.holders",
            "old":false,"new":true,"cause":"props.set","event_seq":2})));
        let reads: Vec<_> = pump(&mut app, &peer, &comp).into_iter()
            .map(|call| serde_json::from_str::<Value>(&call.body).unwrap()["path"].clone())
            .collect();
        assert!(reads.contains(&json!("input.corners.holders")), "the leaf is re-read: {reads:?}");
        let hold = activate_hidden_and_hold(&mut app, &peer, &comp, "panel-left");
        assert_eq!(hold["holder"], "focus");
        assert_eq!(left(&app).active_page_id.as_deref(), Some("beta"));
    }

    #[test]
    fn activate_on_hidden_reveals_with_focus_hold() {
        let comp = FakeComp::new(true);
        let (mut app, peer) = activation_app(&comp);
        let (rc, body) = sub_send(&mut app, &peer, "shell.sub.activate", json!({"name":"beta"}));
        assert_eq!(rc, 0, "{body}");
        assert_eq!(body, json!({"accepted":true,"name":"beta","edge":"left","output":"test",
            "target":null,"focus":true}));
        let panel = left(&app);
        assert_eq!(panel.mode, PanelMode::Hidden, "activation never changes a mode");
        assert!(panel.transient_revealed);
        assert_eq!(panel.active_page_id.as_deref(), Some("beta"));
        assert_eq!(panel.page_change, cosmix_shell::runtime::PageChange::Named);
        // The Panel(Left) focus request: the layer asks for the keyboard.
        assert!(panel.keyboard_requested && !panel.keyboard_focused);
        assert_eq!(panel.keyboard_interactivity, cosmix_shell::runtime::KeyboardInteractivity::Exclusive);

        // The reveal maps the layer: its mode report, then the focus hold —
        // the literal `comp.panel.hold`, addressed to the comp instance.
        // Comp answers the hidden report with a re-stated conceal (nothing
        // holds yet) — delivered after the mode report's ack and before any
        // hold ack, since comp refuses the hold until the layer is mapped:
        // the explicit reveal survives it (the anti-vanish invariant).
        comp.refuse_holds.set(Some("unknown_panel_surface"));
        map_left_layer(&mut app, "panel-left");
        let calls = pump(&mut app, &peer, &comp);
        let commands: Vec<_> = calls.iter().map(|call| (call.to.as_str(), call.command.as_str())).collect();
        assert_eq!(commands, [("comp-nested", "comp.panel.mode"), ("comp-nested", "comp.panel.hold")]);
        assert_eq!(holds(&calls), [json!({"output":"test","edge":"left","surface":"panel-left",
            "holder":"focus","acquire":true})]);
        assert_eq!(comp.published(), [false], "the re-stated conceal verdict");
        assert!(left(&app).transient_revealed, "a conceal before the hold leaves the reveal");
        // The layer maps: the refused hold is sent again and holds the edge.
        comp.refuse_holds.set(None);
        peer.deliver_message(comp_message("surface.mapped", json!({"id":4,"role":"layer","event_seq":3})));
        let acquired = holds(&pump(&mut app, &peer, &comp));
        assert_eq!(acquired.len(), 1);
        assert_eq!(acquired[0]["acquire"], true);
        assert_eq!(comp.published(), [true]);
        assert!(left(&app).transient_revealed);

        // The keyboard lands (the host reports it; comp's focus membership
        // sees it too): the grab drops to on-demand, and the hold has done
        // its job — Quoin releases it and comp's focus holder carries the
        // reveal, so no conceal follows.
        observe_focus(&mut app, Some(Edge::Left));
        comp.set_focused(&peer, true);
        let released = holds(&pump(&mut app, &peer, &comp));
        assert_eq!(released.len(), 1);
        assert_eq!((&released[0]["holder"], &released[0]["acquire"]), (&json!("focus"), &json!(false)));
        assert!(comp.published().is_empty(), "comp's focus membership keeps the verdict");
        let panel = left(&app);
        assert!(panel.transient_revealed && panel.keyboard_focused && panel.keyboard_requested);
        assert_eq!(panel.keyboard_interactivity, cosmix_shell::runtime::KeyboardInteractivity::OnDemand,
            "granted: a click elsewhere can take focus");
        // A comp focus event of any kind no longer bears on the hold.
        peer.deliver_message(focus_changed(Some(5), Some(4)));
        assert!(holds(&pump(&mut app, &peer, &comp)).is_empty());

        // Click-away: focus leaves the panel, the request ends, comp's last
        // holder releases and the reveal ends.
        observe_focus(&mut app, None);
        comp.set_focused(&peer, false);
        pump(&mut app, &peer, &comp);
        assert_eq!(comp.published(), [false]);
        let panel = left(&app);
        assert!(!panel.transient_revealed && !panel.keyboard_requested);
        assert_eq!(panel.mode, PanelMode::Hidden);

        // Escape ends it too, before the keyboard ever landed: the request
        // and the hold both go. Concealment destroyed the layer; the next
        // reveal maps one with a new token.
        unmap_left_layer(&mut app);
        activate_hidden_and_hold(&mut app, &peer, &comp, "panel-left-2");
        keyboard(&mut app, cosmix_shell::runtime::KeyboardCommand::Escape);
        let released = holds(&pump(&mut app, &peer, &comp));
        assert_eq!(released.len(), 1);
        assert_eq!((&released[0]["surface"], &released[0]["acquire"]), (&json!("panel-left-2"), &json!(false)));
        let panel = left(&app);
        assert!(!panel.transient_revealed && !panel.keyboard_requested);
    }

    /// The grant never lands (a lock, a higher Exclusive layer): when the
    /// request times out the reveal the activation made ends with it — the
    /// model hides it and Quoin releases the hold — rather than leaving an
    /// unfocused panel open that Escape (which goes to the application)
    /// cannot reach.
    #[test]
    fn an_ungranted_activation_releases_its_hold_at_the_grant_timeout() {
        let comp = FakeComp::new(true);
        let (mut app, peer) = activation_app(&comp);
        activate_hidden_and_hold(&mut app, &peer, &comp, "panel-left");
        comp.published();
        std::thread::sleep(cosmix_shell::core::FOCUS_GRANT_TIMEOUT + std::time::Duration::from_millis(100));
        let released = holds(&pump(&mut app, &peer, &comp));
        assert_eq!(released.len(), 1);
        assert_eq!(released[0]["acquire"], false);
        assert_eq!(comp.published(), [false], "nothing else holds: comp conceals");
        let panel = left(&app);
        assert!(!panel.transient_revealed && !panel.keyboard_requested);
    }

    /// The session-lock shape (review round 2): comp refuses the hold
    /// `locked` before touching its holder state and publishes nothing, so
    /// no reveal/conceal transition ever reaches Quoin and only the model can
    /// end the reveal. At the grant timeout it does: the activation-made
    /// reveal hides and no hold intent is left. A `focus=false` reveal has no
    /// keyboard request to lapse and keeps the `shell.panel.show` lifecycle.
    #[test]
    fn an_ungranted_activation_under_a_lock_hides_at_the_grant_timeout() {
        let comp = FakeComp::new(true);
        let (mut app, peer) = activation_app(&comp);
        let (rc, body) = sub_send(&mut app, &peer, "shell.sub.activate", json!({"name":"beta"}));
        assert_eq!(rc, 0, "{body}");
        comp.refuse_holds.set(Some("locked"));
        map_left_layer(&mut app, "panel-left");
        let refused = holds(&pump(&mut app, &peer, &comp));
        assert_eq!(refused.len(), 1, "the hold was sent and refused");
        assert_eq!(refused[0]["acquire"], true);
        comp.published();
        assert!(left(&app).transient_revealed);
        std::thread::sleep(cosmix_shell::core::FOCUS_GRANT_TIMEOUT + std::time::Duration::from_millis(100));
        pump(&mut app, &peer, &comp);
        assert!(comp.published().is_empty(), "comp said nothing");
        let panel = left(&app);
        assert!(!panel.transient_revealed && !panel.keyboard_requested, "the model ended it");
        assert_eq!(panel.mode, PanelMode::Hidden);
        assert_eq!(app.world().resource::<crate::holders::HolderClient>().focus_holds(), 0,
            "no hold intent is left");

        // focus=false under the same lock: nothing lapses, the reveal stays.
        let (rc, body) = sub_send(&mut app, &peer, "shell.sub.activate",
            json!({"name":"alpha","focus":false}));
        assert_eq!(rc, 0, "{body}");
        std::thread::sleep(cosmix_shell::core::FOCUS_GRANT_TIMEOUT + std::time::Duration::from_millis(100));
        assert!(holds(&pump(&mut app, &peer, &comp)).is_empty());
        assert!(left(&app).transient_revealed, "a focus=false reveal is untouched");
    }

    /// `focus=false`: reveal or switch for attention, without asking for the
    /// keyboard and without a focus hold — as a header string or a JSON bool.
    #[test]
    fn activate_without_focus_reveals_without_keyboard_or_hold() {
        let comp = FakeComp::new(true);
        let (mut app, peer) = activation_app(&comp);
        map_left_layer(&mut app, "panel-left");
        pump(&mut app, &peer, &comp);
        comp.published();
        let mut header = wire("shell.sub.activate", json!({"name":"beta"}));
        header.headers.insert("focus".into(), "false".into());
        for request in [header, wire("shell.sub.activate", json!({"name":"alpha","focus":false}))] {
            peer.send(request);
            app.update();
            let replies = peer.drain_responses();
            assert_eq!(replies[0].rc, 0, "{}", replies[0].body);
            let body: Value = serde_json::from_str(&replies[0].body).unwrap();
            assert_eq!(body["focus"], false);
            let panel = left(&app);
            assert!(panel.transient_revealed, "revealed for attention");
            assert_eq!(panel.active_page_id.as_deref(), body["name"].as_str());
            assert!(!panel.keyboard_requested);
            assert_eq!(panel.keyboard_interactivity, cosmix_shell::runtime::KeyboardInteractivity::OnDemand);
            assert!(holds(&pump(&mut app, &peer, &comp)).is_empty(), "no focus hold");
        }
    }

    #[test]
    fn activate_on_pinned_or_docked_switches_carousel_only() {
        let comp = FakeComp::new(true);
        let (mut app, peer) = activation_app(&comp);
        map_left_layer(&mut app, "panel-left");
        for mode in [PanelMode::Pinned, PanelMode::Docked] {
            app.world_mut().write_message(ShellCommand {
                output: test_model().output().clone(),
                at: Default::default(),
                kind: ShellCommandKind::Panel { edge: Edge::Left, input: PanelInput::SetMode(mode) },
            });
            pump(&mut app, &peer, &comp);
            for name in ["beta", "alpha"] {
                let before = left(&app);
                assert_eq!(before.mode, mode);
                assert_ne!(before.active_page_id.as_deref(), Some(name));
                let (rc, body) = sub_send(&mut app, &peer, "shell.sub.activate", json!({"name":name}));
                assert_eq!(rc, 0, "{mode:?} {name}: {body}");
                let after = left(&app);
                assert_eq!(after.mode, mode, "{mode:?}: the mode is unchanged");
                assert!(!after.transient_revealed, "{mode:?}: no transient reveal");
                assert_eq!(after.active_page_id.as_deref(), Some(name), "{mode:?}");
                assert_eq!(after.page_change, cosmix_shell::runtime::PageChange::Named,
                    "{mode:?}: a direct jump, never a slide");
                assert!(after.keyboard_requested, "{mode:?}: focus is requested");
                assert_eq!(after.keyboard_interactivity,
                    cosmix_shell::runtime::KeyboardInteractivity::Exclusive,
                    "{mode:?}: the panel asks for the keyboard");
                assert!(holds(&pump(&mut app, &peer, &comp)).is_empty(),
                    "{mode:?}: persistent panels take no hold");
            }
            // The grant lands and the page stays: nothing reverts it.
            observe_focus(&mut app, Some(Edge::Left));
            pump(&mut app, &peer, &comp);
            assert_eq!(left(&app).keyboard_interactivity,
                cosmix_shell::runtime::KeyboardInteractivity::OnDemand);
            observe_focus(&mut app, None);
            pump(&mut app, &peer, &comp);
            let after = left(&app);
            assert_eq!((after.mode, after.active_page_id.as_deref()), (mode, Some("alpha")));
            assert!(!after.keyboard_requested);
        }
    }

    #[test]
    fn activation_targets_focused_window_output_else_pointer() {
        let comp = FakeComp::new(true);
        *comp.focus.borrow_mut() = json!({"keyboard":7,"pointer":9,"window":{"id":7,"generation":1}});
        *comp.outputs.borrow_mut() = BTreeMap::from([(7, "DP-1"), (9, "HDMI-A-1")]);
        let (mut app, peer) = activation_app(&comp);
        let target = |app: &mut App| {
            let (rc, body) = sub_send(app, &peer, "shell.sub.activate", json!({"name":"alpha"}));
            assert_eq!(rc, 0, "{body}");
            assert_eq!(body["output"], "test", "the sub-panel shows on its seat's output");
            body["target"].clone()
        };
        assert_eq!(target(&mut app), "DP-1", "the focused window's output");
        // Nothing focused: the pointer's output. The focus change starts a
        // fresh round of reads, literal `comp.props.get`s to the instance.
        *comp.focus.borrow_mut() = json!({"keyboard":null,"pointer":9});
        peer.deliver_message(focus_changed(None, Some(7)));
        let reads: Vec<_> = pump(&mut app, &peer, &comp).into_iter()
            .filter(|call| call.command == "comp.props.get")
            .map(|call| serde_json::from_str::<Value>(&call.body).unwrap()["path"].clone())
            .collect();
        assert_eq!(reads, [json!("focus"), json!("surfaces.s9.output")]);
        assert_eq!(target(&mut app), "HDMI-A-1");
        // A focused surface comp puts on no output falls back to the pointer.
        *comp.focus.borrow_mut() = json!({"keyboard":11,"pointer":9});
        peer.deliver_message(focus_changed(Some(11), None));
        pump(&mut app, &peer, &comp);
        assert_eq!(target(&mut app), "HDMI-A-1");
        // Neither known: no target, and the activation still stands.
        *comp.focus.borrow_mut() = json!({"keyboard":null,"pointer":null});
        peer.deliver_message(focus_changed(None, Some(11)));
        pump(&mut app, &peer, &comp);
        assert_eq!(target(&mut app), Value::Null);
        // A lost connection forgets where the user was.
        *comp.focus.borrow_mut() = json!({"keyboard":7,"pointer":null});
        peer.deliver_message(focus_changed(Some(7), None));
        pump(&mut app, &peer, &comp);
        assert_eq!(target(&mut app), "DP-1");
        peer.deliver_event(BusBridgeEvent::Connection {
            state: BusConnectionState::Disconnected,
            generation: 1,
        });
        app.update();
        let targets = app.world().resource::<crate::activation::ActivationTargets>();
        assert_eq!(targets.target(), None);
    }
}
