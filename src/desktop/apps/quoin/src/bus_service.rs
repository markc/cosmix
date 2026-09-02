use std::collections::BTreeMap;

use bevy::ecs::message::MessageWriter;
use bevy::prelude::*;
use bevy::time::Real;
use cosmix_props_core::tree::build_snapshot;
use cosmix_props_core::{PropDescribe, PropPath, PropTree, PropType, PropValue};
use cosmix_shell::core::{Edge, PanelMode};
use cosmix_shell::runtime::{
    ShellCommand, ShellFrame, ShellFrameState, ShellRuntimeSet, ShellSemanticVerb,
    semantic_shell_command,
};
use ctk::app_control::authorize_local_caller;
use ctk::bus::{BusBridge, BusBridgeEvent, BusConnectionState, InboundRequest};
use serde_json::{Value, json};

use crate::power::{PowerAction, PowerSync};

/// Bound on replies stashed while the outbound channel is full. Beyond this
/// the oldest is dropped with a warning — a bounded stash that eventually
/// answers beats an unbounded one, and both beat silently losing every reply
/// the moment the channel blinks.
const MAX_PENDING_REPLIES: usize = 32;

#[derive(Component)]
pub(crate) struct QuoinPowerText;

#[derive(Resource)]
struct ShellBusState {
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
    pending_replies: Vec<(InboundRequest, u8, String)>,
}

impl Default for ShellBusState {
    fn default() -> Self {
        Self {
            power: PowerSync::default(),
            ready_logged: false,
            next_request_id: 0x51_0000_0000,
            snapshot_retry: None,
            live_generation: None,
            pending_replies: Vec::new(),
        }
    }
}

pub(crate) struct ShellBusPlugin;

impl Plugin for ShellBusPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ShellBusState>()
            .add_systems(Update, service_bus.in_set(ShellRuntimeSet::Input));
    }
}

fn service_bus(
    bridge: Res<BusBridge>,
    frame: Res<ShellFrameState>,
    time: Res<Time<Real>>,
    mut state: ResMut<ShellBusState>,
    mut shell_commands: MessageWriter<ShellCommand>,
    mut power_text: Query<&mut Text, With<QuoinPowerText>>,
) {
    // This system is the app's single inbound drain + reply owner (see
    // `BusBridge::claim_inbound`); Quoin installs no `AppPortPlugin`.
    bridge.claim_inbound("quoin shell service");

    if let Some(generation) = state.snapshot_retry.take() {
        request_power_snapshot(&bridge, &mut state, generation);
    }

    let mut power_changed = false;
    for event in bridge.drain_events() {
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
                state.power.invalidate();
                state.snapshot_retry = None;
                state.live_generation = None;
                // Replies stashed under the epoch that just ended: the worker
                // drops a response stamped with a stale generation anyway, so
                // retrying them only re-fires dead sends.
                state.pending_replies.clear();
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
    if power_changed {
        let rendered = state.power.render();
        for mut text in &mut power_text {
            **text = rendered.clone();
        }
    }

    // Retry stashed replies before answering new work so a recovered channel
    // drains in arrival order.
    let pending = std::mem::take(&mut state.pending_replies);
    for (request, rc, body) in pending {
        stash_or_respond(&bridge, &mut state, request, rc, body);
    }

    for request in bridge.drain_inbound() {
        let (rc, body, command) = dispatch_shell_request(&request, &frame.0, time.elapsed());
        if let Some(command) = command {
            shell_commands.write(command);
        }
        stash_or_respond(&bridge, &mut state, request, rc, body);
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
fn stash_or_respond(
    bridge: &BusBridge,
    state: &mut ShellBusState,
    request: InboundRequest,
    rc: u8,
    body: String,
) {
    if let Err(error) = bridge.try_respond(&request, rc, body.clone()) {
        if bridge.worker_is_gone() {
            bevy::log::warn!(
                command = request.command.as_str(),
                "shell Bus worker has stopped; dropping reply ({error})"
            );
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
        state.pending_replies.push((request, rc, body));
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
            "verbs":["panel.show","panel.hide","panel.toggle","panel.pin","panel.unpin","panel.page.next","panel.page.prev","panel.page.set"]
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
    // The mutation gate. CROSS-COMPONENT TRUST DEPENDENCY: this authorization
    // is only as strong as noded's guarantee to strip client-supplied
    // `broker_origin`/identity headers and restamp them from connection
    // state. If noded ever forwards a client's own header spelling, every
    // mesh peer gains unauthenticated mutation of the desktop shell, and no
    // test in this repository can catch it — the invariant lives in noded and
    // must be enforced (and tested) there. Failure the other way (noded stops
    // stamping) fails closed here.
    if let Err(error) = authorize_local_caller(request) {
        return (
            10,
            json!({"error":format!("local registered caller required: {error:?}")}).to_string(),
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
        "shell.panel.show" => ShellSemanticVerb::PanelShow,
        "shell.panel.hide" => ShellSemanticVerb::PanelHide,
        "shell.panel.toggle" => ShellSemanticVerb::PanelToggle,
        "shell.panel.pin" => ShellSemanticVerb::PanelPin,
        "shell.panel.unpin" => ShellSemanticVerb::PanelUnpin,
        "shell.panel.page.next" => ShellSemanticVerb::PageNext,
        "shell.panel.page.prev" => ShellSemanticVerb::PagePrevious,
        "shell.panel.page.set" => ShellSemanticVerb::PageSet(argument(request, "id")?),
        _ => return None,
    })
}

fn parse_args(request: &InboundRequest) -> Option<Value> {
    serde_json::from_str(&request.body).ok()
}

fn argument(request: &InboundRequest, name: &str) -> Option<String> {
    request
        .headers
        .get(name)
        .cloned()
        .or_else(|| parse_args(request)?.get(name)?.as_str().map(str::to_owned))
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
    fn app_verbs_and_bad_page_arguments_get_precise_errors() {
        let frame = test_frame();
        let (rc, body, command) =
            dispatch_shell_request(&request("app.ping"), &frame, Default::default());
        assert_eq!(rc, 10);
        assert!(body.contains("not supported by service shell"), "{body}");
        assert!(command.is_none());

        let (rc, body, _) =
            dispatch_shell_request(&local("shell.panel.page.set"), &frame, Default::default());
        assert_eq!(rc, 10);
        assert!(body.contains("requires an id"), "{body}");

        let mut set = local("shell.panel.page.set");
        set.headers.insert("id".to_owned(), "no-such".to_owned());
        let (rc, body, command) = dispatch_shell_request(&set, &frame, Default::default());
        assert_eq!(rc, 10);
        assert!(body.contains("unknown page id"), "{body}");
        assert!(
            command.is_none(),
            "an unacceptable verb must not be enqueued"
        );
    }

    /// An `App` carrying just what `service_bus` reads, driven by a bridge
    /// with no worker behind it.
    fn bus_app(bridge: BusBridge) -> App {
        let mut app = App::new();
        app.add_plugins(bevy::MinimalPlugins)
            .add_message::<ShellCommand>()
            .init_resource::<ShellBusState>()
            .insert_resource(ShellFrameState(test_frame()))
            .insert_resource(bridge)
            .add_systems(Update, service_bus);
        app
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
        assert_eq!(calls.len(), 1, "the Connected event issues one snapshot");
        assert_eq!(calls[0].command, "power.props.get");
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
            peer.drain_calls().len(),
            2,
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

    fn test_frame() -> ShellFrame {
        let model = cosmix_shell::core::ShellModel::new(
            cosmix_shell::core::OutputKey::new("test").unwrap(),
            cosmix_shell::core::LogicalSize::new(1000.0, 800.0).unwrap(),
            Default::default(),
            std::time::Duration::from_millis(800),
            std::time::Duration::from_millis(200),
        )
        .unwrap();
        ShellFrame::from_model(&model)
    }
}
