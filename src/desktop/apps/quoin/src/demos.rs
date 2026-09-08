//! Keeper backgrounds and explicit full-output capture through the shared Bus.
use bevy::{
    input_focus::{InputFocus, tab_navigation::TabIndex},
    picking::hover::Hovered,
    prelude::*,
    ui::InteractionDisabled,
    ui_widgets::{Activate, Button as WidgetButton},
};
use cosmix_shell::{
    core::Edge,
    runtime::{ShellFrameState, ShellRuntimeSet},
};
use cosmix_shell_host::LayerHostDeadline;
use ctk::{
    bus::{BusBridge, BusBridgeEvent, BusConnectionState},
    theme::tokens,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};

#[derive(Clone)]
struct Call {
    verb: &'static str,
    body: Value,
}
#[derive(Default)]
struct Lane {
    snapshot: Option<Value>,
    pending: Option<(u64, bool, Duration)>,
    queued: Option<Call>,
    refresh: Duration,
    feedback: String,
}
impl Lane {
    fn can_act(&self) -> bool {
        self.snapshot.is_some()
            && self.queued.is_none()
            && !self.pending.is_some_and(|(_, action, _)| action)
    }
    fn queue(&mut self, verb: &'static str, body: Value) {
        if self.can_act() {
            self.queued = Some(Call { verb, body });
            self.feedback = "Working…".into();
        }
    }
    fn event(&mut self, event: &BusBridgeEvent, now: Duration) {
        if let BusBridgeEvent::Reply { request_id, result } = event {
            let Some((id, action, _)) = self.pending else {
                return;
            };
            if *request_id != id {
                return;
            }
            self.pending = None;
            match result {
                Ok(reply) if reply.rc == 0 => match serde_json::from_str::<Value>(&reply.body) {
                    Ok(value) if value.is_object() => {
                        if !action {
                            self.snapshot = Some(value);
                            if self.queued.is_none() {
                                self.feedback.clear();
                            }
                        }
                        self.refresh = if action {
                            now
                        } else {
                            now + Duration::from_secs(1)
                        };
                        if action {
                            self.feedback.clear();
                            self.snapshot = None;
                        }
                    }
                    _ => {
                        self.snapshot = None;
                        self.feedback = "Invalid service reply".into();
                        self.refresh = now + Duration::from_secs(2);
                    }
                },
                other => {
                    self.feedback = match other {
                        Ok(reply) => reply.body.clone(),
                        Err(error) => error.to_string(),
                    };
                    self.snapshot = None;
                    self.refresh = now + Duration::from_secs(2);
                }
            }
        }
    }
    fn tick(
        &mut self,
        bridge: &BusBridge,
        now: Duration,
        id: &mut u64,
        target: &str,
        status: &'static str,
        deadline: &mut LayerHostDeadline,
    ) {
        if self.pending.is_some_and(|(_, _, at)| now >= at) {
            self.pending = None;
            self.snapshot = None;
            self.queued = None;
            self.feedback = "Timed out; checking service state".into();
            self.refresh = now;
        }
        if self.pending.is_none() && (self.queued.is_some() || now >= self.refresh) {
            *id += 1;
            let action = self.queued.is_some();
            let call = self.queued.clone().unwrap_or(Call {
                verb: status,
                body: json!({}),
            });
            if bridge
                .try_call(
                    *id,
                    target,
                    call.verb,
                    BTreeMap::new(),
                    call.body.to_string(),
                )
                .is_ok()
            {
                self.queued = None;
                self.pending = Some((*id, action, now + Duration::from_secs(3)));
            } else {
                self.refresh = now + Duration::from_millis(250);
            }
        }
        let at = self.pending.map_or(self.refresh, |(_, _, at)| at);
        deadline.0 = Some(deadline.0.map_or(at, |existing| existing.min(at)));
    }
}

#[derive(Resource)]
pub(crate) struct DemoState {
    connected: bool,
    next: u64,
    background: Lane,
    capture: Lane,
}
impl Default for DemoState {
    fn default() -> Self {
        Self {
            connected: false,
            next: 0x58_0000_0000,
            background: Lane::default(),
            capture: Lane::default(),
        }
    }
}
impl DemoState {
    pub(crate) fn event(&mut self, event: &BusBridgeEvent, now: Duration) {
        match event {
            BusBridgeEvent::Connection { state, .. } => {
                self.connected = *state == BusConnectionState::Connected;
                self.background = Lane::default();
                self.capture = Lane::default();
            }
            BusBridgeEvent::Fatal(_) => {
                self.connected = false;
                self.background = Lane::default();
                self.capture = Lane::default();
            }
            _ => {
                self.background.event(event, now);
                self.capture.event(event, now);
            }
        }
    }
    pub(crate) fn tick(
        &mut self,
        bridge: &BusBridge,
        now: Duration,
        deadline: &mut LayerHostDeadline,
    ) {
        if !self.connected {
            return;
        }
        self.background.tick(
            bridge,
            now,
            &mut self.next,
            "bg-showcase",
            "background.status",
            deadline,
        );
        self.capture.tick(
            bridge,
            now,
            &mut self.next,
            "capture",
            "capture.status",
            deadline,
        );
    }
}

pub(crate) struct DemoPlugin;
impl Plugin for DemoPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(activate)
            .add_systems(Update, present.in_set(ShellRuntimeSet::Host));
    }
}
#[derive(Component, Clone, Copy)]
enum Action {
    Scene(&'static str),
    Camera,
    Kick,
    Screenshot,
    Record,
}
#[derive(Component)]
struct Feedback(bool);
#[derive(Component)]
struct ActionLabel(Action);

fn visible(frame: &ShellFrameState) -> bool {
    let panel = frame.0.panel(Edge::Right);
    panel.mapped && panel.active_page_id.as_deref() == Some("demos")
}
fn text(commands: &mut Commands, label: &str) -> Entity {
    commands
        .spawn((
            Text::new(label),
            bevy::feathers::theme::ThemeTextColor(tokens::TEXT),
            TextFont {
                font_size: FontSize::Px(13.0),
                ..default()
            },
        ))
        .id()
}
pub(crate) fn controls(commands: &mut Commands) -> Entity {
    let root = commands
        .spawn(Node {
            flex_direction: FlexDirection::Column,
            row_gap: px(6),
            padding: UiRect::all(px(8)),
            ..default()
        })
        .id();
    let title = text(commands, "Background demos");
    commands.entity(root).add_child(title);
    for (action, label) in [
        (Action::Scene("bloom"), "Bloom"),
        (Action::Scene("shapes"), "Shapes"),
        (Action::Scene("boing"), "Boing"),
        (Action::Scene("boids"), "Boids"),
        (Action::Camera, "Camera: fixed"),
        (Action::Kick, "Kick Boing · F9"),
        (Action::Screenshot, "Save screenshot"),
        (Action::Record, "Record full screen MP4"),
    ] {
        let button = commands
            .spawn((
                Node {
                    min_height: px(28),
                    padding: UiRect::axes(px(8), px(4)),
                    ..default()
                },
                bevy::feathers::theme::ThemeBackgroundColor(tokens::CONTROL),
                WidgetButton,
                Pickable::default(),
                Hovered::default(),
                TabIndex(-1),
                InteractionDisabled,
                action,
            ))
            .id();
        let label = text(commands, label);
        commands.entity(label).insert(ActionLabel(action));
        commands.entity(button).add_child(label);
        commands.entity(root).add_child(button);
    }
    for capture in [false, true] {
        let label = text(commands, "Connecting…");
        commands.entity(label).insert((
            Feedback(capture),
            TextLayout {
                linebreak: bevy::text::LineBreak::AnyCharacter,
                ..default()
            },
            Node {
                max_width: percent(100),
                min_width: px(0),
                ..default()
            },
        ));
        commands.entity(root).add_child(label);
    }
    let hint = text(
        commands,
        "Captures include Quoin and open windows. Stop recording to finish the MP4.",
    );
    commands.entity(root).add_child(hint);
    root
}
fn allowed(state: &DemoState, action: Action) -> bool {
    let scene = state
        .background
        .snapshot
        .as_ref()
        .and_then(|v| v["scene"].as_str());
    match action {
        Action::Scene(_) => state.background.can_act(),
        Action::Camera => {
            state.background.can_act() && matches!(scene, Some("boing" | "bloom" | "shapes"))
        }
        Action::Kick => state.background.can_act() && scene == Some("boing"),
        Action::Screenshot | Action::Record => {
            state.capture.can_act()
                && state.capture.snapshot.as_ref().is_some_and(|v| {
                    let phase = v["phase"].as_str().unwrap_or("");
                    matches!(phase, "idle" | "complete" | "failed")
                        || (matches!(action, Action::Record)
                            && matches!(phase, "starting" | "recording"))
                })
        }
    }
}
fn activate(
    event: On<Activate>,
    buttons: Query<(&Action, Has<InteractionDisabled>)>,
    frame: Res<ShellFrameState>,
    mut state: ResMut<DemoState>,
    mut redraw: MessageWriter<bevy::window::RequestRedraw>,
) {
    let Ok((&action, disabled)) = buttons.get(event.entity) else {
        return;
    };
    if disabled || !visible(&frame) || !allowed(&state, action) {
        return;
    }
    match action {
        Action::Scene(scene) => state
            .background
            .queue("background.select", json!({"scene":scene,"camera":"fixed"})),
        Action::Camera => {
            let v = state.background.snapshot.as_ref().unwrap();
            let body = json!({"scene":v["scene"],"camera":if v["camera"]=="orbit" {"fixed"} else {"orbit"}});
            state.background.queue("background.select", body);
        }
        Action::Kick => state.background.queue("boing.kick", json!({})),
        Action::Screenshot => state.capture.queue("capture.screenshot", json!({})),
        Action::Record => {
            let recording = state.capture.snapshot.as_ref().unwrap()["recording"]
                .as_bool()
                .unwrap_or(false);
            state.capture.queue(
                if recording {
                    "capture.stop"
                } else {
                    "capture.start"
                },
                if recording {
                    json!({})
                } else {
                    json!({"fps":30})
                },
            );
        }
    }
    redraw.write(bevy::window::RequestRedraw);
}
fn present(
    mut commands: Commands,
    frame: Res<ShellFrameState>,
    state: Res<DemoState>,
    mut focus: ResMut<InputFocus>,
    mut buttons: Query<(Entity, &Action, &mut TabIndex, Has<InteractionDisabled>)>,
    mut labels: Query<(&ActionLabel, &mut Text), Without<Feedback>>,
    mut feedback: Query<(&Feedback, &mut Text), Without<ActionLabel>>,
) {
    for (entity, &action, mut tab, disabled) in &mut buttons {
        let enabled = visible(&frame) && allowed(&state, action);
        let wanted = if enabled { 0 } else { -1 };
        if tab.0 != wanted {
            tab.0 = wanted;
        }
        if enabled && disabled {
            commands.entity(entity).remove::<InteractionDisabled>();
        } else if !enabled && !disabled {
            commands.entity(entity).insert(InteractionDisabled);
        }
        if !visible(&frame) && focus.get() == Some(entity) {
            focus.clear();
        }
    }
    for (ActionLabel(action), mut label) in &mut labels {
        let next = match action {
            Action::Camera => format!(
                "Camera: {}",
                state
                    .background
                    .snapshot
                    .as_ref()
                    .and_then(|v| v["camera"].as_str())
                    .unwrap_or("—")
            ),
            Action::Record => {
                if state
                    .capture
                    .snapshot
                    .as_ref()
                    .is_some_and(|v| v["recording"] == true)
                {
                    "Stop recording MP4".into()
                } else {
                    "Record full screen MP4".into()
                }
            }
            _ => continue,
        };
        if label.0 != next {
            label.0 = next;
        }
    }
    for (Feedback(capture), mut label) in &mut feedback {
        let lane = if *capture {
            &state.capture
        } else {
            &state.background
        };
        let next = if !lane.feedback.is_empty() {
            lane.feedback.clone()
        } else if let Some(v) = &lane.snapshot {
            if *capture {
                format!(
                    "Capture: {}\n{}{}",
                    v["phase"].as_str().unwrap_or("unknown"),
                    v["path"].as_str().unwrap_or(""),
                    v["error"]
                        .as_str()
                        .map(|e| format!("\n{e}"))
                        .unwrap_or_default()
                )
            } else {
                format!("Showing: {}", v["scene"].as_str().unwrap_or("unknown"))
            }
        } else {
            if *capture {
                "Capture unavailable"
            } else {
                "Background unavailable"
            }
            .into()
        };
        if label.0 != next {
            label.0 = next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ctk::bus::{BusReply, test_bridge};
    #[test]
    fn action_ack_requires_status_and_old_reply_is_ignored() {
        let (bridge, peer) = test_bridge("shell");
        let mut state = DemoState::default();
        state.event(
            &BusBridgeEvent::Connection {
                state: BusConnectionState::Connected,
                generation: 1,
            },
            Duration::ZERO,
        );
        let mut deadline = LayerHostDeadline::default();
        state.tick(&bridge, Duration::ZERO, &mut deadline);
        let calls = peer.drain_calls();
        assert_eq!(calls.len(), 2);
        let reply = |id, value: Value| BusBridgeEvent::Reply {
            request_id: id,
            result: Ok(BusReply {
                rc: 0,
                body: value.to_string(),
                result: None,
            }),
        };
        state.event(
            &reply(
                calls[0].request_id,
                json!({"scene":"boing","camera":"fixed","outputs":1}),
            ),
            Duration::ZERO,
        );
        assert!(allowed(&state, Action::Kick));
        state
            .background
            .queue("background.select", json!({"scene":"bloom"}));
        state.tick(&bridge, Duration::ZERO, &mut deadline);
        let call = peer.drain_calls().remove(0);
        state.event(
            &reply(calls[0].request_id, json!({"scene":"boids"})),
            Duration::ZERO,
        );
        assert_eq!(
            state.background.snapshot.as_ref().unwrap()["scene"],
            "boing"
        );
        state.event(
            &reply(call.request_id, json!({"accepted":true,"scene":"bloom"})),
            Duration::ZERO,
        );
        state.tick(&bridge, Duration::ZERO, &mut deadline);
        assert_eq!(peer.drain_calls()[0].command, "background.status");
    }
    #[test]
    fn recovered_status_clears_errors_and_reports_completed_capture() {
        let mut lane = Lane {
            pending: Some((1, false, Duration::from_secs(3))),
            ..Default::default()
        };
        lane.event(
            &BusBridgeEvent::Reply {
                request_id: 1,
                result: Err("service starting".into()),
            },
            Duration::ZERO,
        );
        assert!(!lane.feedback.is_empty());
        lane.pending = Some((2, false, Duration::from_secs(3)));
        lane.event(
            &BusBridgeEvent::Reply {
                request_id: 2,
                result: Ok(BusReply {
                    rc: 0,
                    body: json!({"phase":"complete","recording":false,"path":"/captures/demo.mp4"})
                        .to_string(),
                    result: None,
                }),
            },
            Duration::ZERO,
        );
        assert!(lane.feedback.is_empty());
        assert_eq!(lane.snapshot.unwrap()["path"], "/captures/demo.mp4");
    }
    #[test]
    fn finalising_is_not_a_saved_file_or_a_new_recording_opportunity() {
        let mut state = DemoState::default();
        state.capture.snapshot =
            Some(json!({"recording":false,"phase":"finalising","path":"pending.mp4"}));
        assert!(!allowed(&state, Action::Record));
        assert!(!allowed(&state, Action::Screenshot));
        state.capture.snapshot = Some(json!({"recording":true,"phase":"recording"}));
        assert!(allowed(&state, Action::Record));
        assert!(!allowed(&state, Action::Screenshot));
    }
}
