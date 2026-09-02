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

use crate::power::PowerSync;

const POWER_TOPIC: &str = "power.props.changed";

#[derive(Component)]
pub(crate) struct QuoinPowerText;

#[derive(Resource)]
struct ShellBusState {
    power: PowerSync,
    ready_logged: bool,
    next_request_id: u64,
}

impl Default for ShellBusState {
    fn default() -> Self {
        Self {
            power: PowerSync::default(),
            ready_logged: false,
            next_request_id: 0x51_0000_0000,
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
                request_power_snapshot(&bridge, &mut state, generation);
                power_changed = true;
            }
            BusBridgeEvent::Connection { .. } | BusBridgeEvent::Fatal(_) => {
                state.power.invalidate();
                power_changed = true;
            }
            BusBridgeEvent::Reply { request_id, result } => {
                power_changed |= state.power.accept_reply(request_id, result);
            }
            BusBridgeEvent::DroppedMessages(_) => {
                if let Some(generation) = state.power.generation() {
                    request_power_snapshot(&bridge, &mut state, generation);
                } else {
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
        if message.topic() == Some(POWER_TOPIC)
            && message
                .headers
                .get("gap")
                .is_some_and(|value| value == "true")
        {
            if let Some(generation) = state.power.generation() {
                request_power_snapshot(&bridge, &mut state, generation);
            } else {
                state.power.invalidate();
            }
            power_changed = true;
        } else {
            power_changed |= state.power.accept_message(message);
        }
    }
    if power_changed {
        let rendered = state.power.render();
        for mut text in &mut power_text {
            **text = rendered.clone();
        }
    }

    for request in bridge.drain_inbound() {
        let (rc, body, command) = dispatch_shell_request(&request, &frame.0, time.elapsed());
        if let Some(command) = command {
            shell_commands.write(command);
        }
        if let Err(error) = bridge.try_respond(&request, rc, body) {
            bevy::log::warn!("shell Bus response dropped: {error}");
        }
    }
}

fn request_power_snapshot(bridge: &BusBridge, state: &mut ShellBusState, generation: u64) {
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
        state.power.invalidate();
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
    let Some(verb) = semantic_verb(request) else {
        return (
            10,
            json!({"error":"unknown shell command"}).to_string(),
            None,
        );
    };
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
    let command = semantic_shell_command(frame, frame.geometry.output.clone(), at, edge, verb);
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

    #[test]
    fn read_surface_is_open_but_semantic_verbs_require_local_registration() {
        let frame = test_frame();
        let request = |command: &str| InboundRequest {
            connection_generation: 1,
            from: "peer".to_owned(),
            command: command.to_owned(),
            headers: BTreeMap::new(),
            body: r#"{"edge":"left"}"#.to_owned(),
            reply_id: Some("1".to_owned()),
        };
        assert_eq!(
            dispatch_shell_request(&request("shell.ping"), &frame, Default::default()).0,
            0
        );
        assert_eq!(
            dispatch_shell_request(&request("shell.panel.show"), &frame, Default::default()).0,
            10
        );
        let mut local = request("shell.panel.show");
        local
            .headers
            .insert("broker_origin".to_owned(), "local".to_owned());
        assert_eq!(
            dispatch_shell_request(&local, &frame, Default::default()).0,
            0
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
