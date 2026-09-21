use std::collections::BTreeMap;

use bevy::ecs::message::MessageWriter;
use bevy::prelude::*;
use bevy::time::Real;
use cosmix_props_core::tree::build_snapshot;
use cosmix_props_core::{PropDescribe, PropPath, PropTree, PropType, PropValue};
use cosmix_shell::core::{Corner, Edge, PanelMode};
use cosmix_shell::runtime::{
    ShellCommand, ShellCommandKind, ShellFrame, ShellFrameState, ShellRuntimeSet,
    ShellSemanticVerb, semantic_shell_command,
};
use ctk::app_control::verify_caller_provenance;
use ctk::bus::{BusBridge, BusBridgeEvent, BusConnectionState, InboundRequest};
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

impl Plugin for ShellBusPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ShellBusState>()
            .init_resource::<cosmix_scene_bevy::SceneStore>()
            .init_resource::<cosmix_scene_bevy::SceneEvents>()
            .init_resource::<crate::wallpaper::WallpaperState>()
            .init_resource::<crate::demos::DemoState>()
            .init_resource::<cosmix_shell_host::LayerHostDeadline>()
            .add_message::<cosmix_shell::runtime::ShellResizeResult>()
            .add_systems(Update, service_bus.in_set(ShellRuntimeSet::Input))
            .add_systems(Update, reply_resizes.in_set(ShellRuntimeSet::Presentation));
    }
}

#[derive(bevy::ecs::system::SystemParam)]
struct SceneBus<'w, 's> {
    power_text: Query<'w, 's, &'static mut Text, With<QuoinPowerText>>,
    scenes: ResMut<'w, cosmix_scene_bevy::SceneStore>,
    events: ResMut<'w, cosmix_scene_bevy::SceneEvents>,
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
                    println!("QUOIN_BUS_READY service=shell");
                    state.ready_logged = true;
                }
                state.live_generation = Some(generation);
                request_power_snapshot(&bridge, &mut state, generation);
                power_changed = true;
            }
            BusBridgeEvent::Connection { .. } | BusBridgeEvent::Fatal(_) => {
                state.pending_resizes.clear();
                state.power.invalidate();
                state.snapshot_retry = None;
                state.live_generation = None;
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
                power_changed |= state.power.accept_reply(request_id, result);
            }
            BusBridgeEvent::DroppedMessages(_) => {
                if let Some(generation) = state.power.generation() {
                    request_power_snapshot(&bridge, &mut state, generation);
                } else {
                    // No generation to key a sync on; MAJOR-1 recovery kicks
                    // in on the next delivered change instead.
                    state.power.invalidate();
                }
                power_changed = true;
            }
            BusBridgeEvent::ObservationConnection { .. }
            | BusBridgeEvent::ObservationReply { .. }
            | BusBridgeEvent::ObservationDroppedMessages(_) => {}
        }
    }
    for message in bridge.drain_messages() {
        wallpaper.0.message(&message, time.elapsed());
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
        let (rc, body, command) =
            if let Some(verb) = cosmix_shell::runtime::SceneVerb::parse(&request.command) {
                let args = parse_args(&request).unwrap_or(Value::Null);
                let (rc, body) = content.scenes.dispatch(verb, &request.body, &args, &bridge);
                (rc, body, None)
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
            "verbs":["quit","panel.show","panel.hide","panel.toggle","panel.pin","panel.unpin","panel.resize","panel.page.next","panel.page.prev","panel.page.set","corner.show","corner.hide","corner.toggle","corner.pin","corner.unpin","debug.status","scene.load","scene.patch","scene.get","scene.describe","scene.unload","scene.watch"],
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
    // beside it worked fine. That is a "who may" gate by the law's own test,
    // and it protected nothing: a caller already on the local Bus can signal
    // this process directly.
    //
    // CROSS-COMPONENT TRUST DEPENDENCY, unchanged and still load-bearing:
    // what remains is only as strong as noded's guarantee to strip
    // client-supplied `broker_origin`/identity headers and restamp them from
    // connection state. A self-asserted `source_peer`/`permissions`/
    // `signed_ident`, a missing stamp, or a duplicated one is still refused,
    // because each says the stamp cannot be trusted — not that the caller is
    // the wrong one. Absence fails closed. The correctness checks below (edge
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
fn argument(request: &InboundRequest, name: &str) -> Option<String> {
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
fn number_argument(request: &InboundRequest, name: &str) -> Option<f64> {
    let value = parse_args(request)?;
    let field = value.get(name)?;
    let number = field
        .as_f64()
        .or_else(|| field.as_str().and_then(|text| text.parse::<f64>().ok()))?;
    number.is_finite().then_some(number)
}

fn parse_edge(value: String) -> Option<Edge> {
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
                    (panel.mode == PanelMode::Pinned).into(),
                ),
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
            for field in ["visible", "pinned", "width_px", "page", "pages", "output"] {
                paths.push(PropPath::new(format!("panels.{}.{}", edge_name(edge), field)).unwrap());
            }
        }
        paths
    }

    fn describe(&self, path: &PropPath) -> Option<PropDescribe> {
        let field = path.as_str().rsplit('.').next()?;
        let ty = match field {
            "visible" | "pinned" => PropType::Bool,
            "width_px" => PropType::Number,
            "page" | "output" => PropType::String,
            "pages" => PropType::List,
            _ => return None,
        };
        Some(PropDescribe::leaf(
            path.clone(),
            ty,
            "live Quoin panel state",
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
    use ctk::bus::{BusMessage, test_bridge};

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
            let (rc, body, _) = dispatch_shell_request(
                &unregistered(command),
                &frame,
                std::time::Duration::ZERO,
            );
            assert_eq!(rc, 0, "{command} refused an unregistered local caller: {body}");
        }

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

    #[test]
    fn read_surface_is_open_but_semantic_verbs_require_local_registration() {
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

    #[test]
    fn corner_actions_share_panel_semantics_and_require_registered_callers() {
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
        for (command, expected) in [
            (
                "shell.panel.show",
                ShellCommandKind::Panel {
                    edge: Edge::Bottom,
                    input: PanelInput::Reveal,
                },
            ),
            (
                "shell.panel.hide",
                ShellCommandKind::Panel {
                    edge: Edge::Bottom,
                    input: PanelInput::Hide,
                },
            ),
            (
                "shell.panel.toggle",
                ShellCommandKind::Panel {
                    edge: Edge::Bottom,
                    input: PanelInput::Toggle,
                },
            ),
            (
                "shell.panel.pin",
                ShellCommandKind::Panel {
                    edge: Edge::Bottom,
                    input: PanelInput::Pin,
                },
            ),
            (
                "shell.panel.unpin",
                ShellCommandKind::Panel {
                    edge: Edge::Bottom,
                    input: PanelInput::Unpin,
                },
            ),
            (
                "shell.panel.page.next",
                ShellCommandKind::Carousel {
                    edge: Edge::Bottom,
                    input: CarouselInput::Next,
                },
            ),
            (
                "shell.panel.page.prev",
                ShellCommandKind::Carousel {
                    edge: Edge::Bottom,
                    input: CarouselInput::Previous,
                },
            ),
        ] {
            let request = wire(command, json!({"edge":"bottom"}));
            let (rc, body, enqueued) = dispatch_shell_request(&request, &frame, Default::default());
            assert_eq!(rc, 0, "{command}: {body}");
            assert_eq!(
                enqueued.expect("accepted verb enqueues a command").kind,
                expected,
                "{command}"
            );
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
        assert_eq!(calls.len(), 1, "a live-generation resync must be honored");
        assert_eq!(calls[0].command, "power.props.get");
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
                "power.props.get",
                "wallpaper.props.get",
                "background.status",
                "capture.status"
            ],
            "drain_events must run before drain_messages"
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
