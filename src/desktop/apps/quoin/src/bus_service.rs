use std::collections::BTreeMap;

use bevy::ecs::message::MessageWriter;
use bevy::prelude::*;
use bevy::time::Real;
use cosmix_props_core::tree::build_snapshot;
use cosmix_props_core::{PropDescribe, PropPath, PropTree, PropType, PropValue};
use cosmix_shell::core::{Edge, PanelMode};
use cosmix_shell::runtime::{
    ShellCommand, ShellCommandKind, ShellFrame, ShellFrameState, ShellRuntimeSet,
    ShellSemanticVerb, semantic_shell_command,
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
    pending_replies: Vec<(InboundRequest, u8, String, Option<ShellCommand>)>,
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
        let (rc, body, command) = dispatch_shell_request(&request, &frame.0, time.elapsed());
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
            "verbs":["quit","panel.show","panel.hide","panel.toggle","panel.pin","panel.unpin","panel.resize","panel.page.next","panel.page.prev","panel.page.set"]
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
        if let Err(error) = authorize_local_caller(request) {
            return (
                10,
                json!({"error":format!("local registered caller required: {error:?}")}).to_string(),
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
/// `signed_ident` are read straight off it by [`authorize_local_caller`]), so
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
            10
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
