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

use crate::wallpaper::{RETRY_INITIAL, arm, next_backoff};

#[derive(Clone)]
struct Call {
    verb: &'static str,
    body: Value,
}
/// Neither `bg-showcase` (`background.status`) nor `capture`
/// (`capture.status`) publishes a change notification for the state this
/// page shows, and capture phases move on their own (a recording finishes,
/// auto-stops at 300 s, finalises). So while — and only while — the page is
/// visible, status is re-read no faster than this; hidden, nothing polls and
/// opening the page reads immediately.
const VISIBLE_REFRESH: Duration = Duration::from_secs(5);

struct Lane {
    snapshot: Option<Value>,
    pending: Option<(u64, bool, Duration)>,
    queued: Option<Call>,
    /// When the next status read is due.
    refresh: Option<Duration>,
    /// Delay applied after the next failed read.
    backoff: Duration,
    /// A status read owed regardless of page visibility: the connection's
    /// bootstrap and the readback after an action. Cleared on send, so a
    /// bootstrap that fails while hidden is not retried hidden: opening the
    /// tab reads anyway, and nothing on screen shows the stale value.
    owed: bool,
    feedback: String,
}
impl Default for Lane {
    fn default() -> Self {
        Self {
            snapshot: None,
            pending: None,
            queued: None,
            refresh: Some(Duration::ZERO),
            backoff: RETRY_INITIAL,
            owed: true,
            feedback: String::new(),
        }
    }
}
impl Lane {
    fn failed(&mut self, now: Duration) {
        self.snapshot = None;
        self.refresh = Some(now + self.backoff);
        self.backoff = next_backoff(self.backoff);
    }
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
                        if action {
                            // Read the action's effect back straight away.
                            self.refresh = Some(now);
                            self.owed = true;
                            self.feedback.clear();
                            self.snapshot = None;
                        } else {
                            self.refresh = Some(now + VISIBLE_REFRESH);
                            self.backoff = RETRY_INITIAL;
                        }
                    }
                    _ => {
                        self.feedback = "Invalid service reply".into();
                        self.owed |= action;
                        self.failed(now);
                    }
                },
                other => {
                    self.feedback = match other {
                        Ok(reply) => reply.body.clone(),
                        Err(error) => error.to_string(),
                    };
                    self.owed |= action;
                    self.failed(now);
                }
            }
        }
    }
    fn tick(
        &mut self,
        bridge: &BusBridge,
        now: Duration,
        id: &mut u64,
        (target, status): (&str, &'static str),
        visible: bool,
        deadline: &mut LayerHostDeadline,
    ) {
        if let Some((_, action, at)) = self.pending
            && now >= at
        {
            self.pending = None;
            self.snapshot = None;
            self.queued = None;
            self.feedback = "Timed out; checking service state".into();
            if action {
                // An unconfirmed action owes its status readback at once.
                self.refresh = Some(now);
                self.owed = true;
            } else {
                // A hung producer is a failed read: back off, visible-only.
                self.failed(now);
            }
        }
        let may_read = visible || self.owed;
        let due = may_read && self.refresh.is_some_and(|at| now >= at);
        if self.pending.is_none() && (self.queued.is_some() || due) {
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
                if !action {
                    self.refresh = None;
                    self.owed = false;
                }
                self.queued = None;
                self.pending = Some((*id, action, now + Duration::from_secs(3)));
            } else {
                // Bridge queue full: a local condition, retried shortly.
                self.refresh = Some(now + Duration::from_millis(250));
            }
        }
        if let Some((_, _, at)) = self.pending {
            arm(deadline, at);
        } else if let Some(at) = self.refresh
            && (may_read || self.queued.is_some())
        {
            arm(deadline, at);
        }
    }
}

#[derive(Resource)]
pub(crate) struct DemoState {
    connected: bool,
    next: u64,
    background: Lane,
    capture: Lane,
    /// Last page visibility seen by `present` (post-model, same update).
    visible: bool,
}
impl Default for DemoState {
    fn default() -> Self {
        Self {
            connected: false,
            next: 0x58_0000_0000,
            background: Lane::default(),
            capture: Lane::default(),
            visible: false,
        }
    }
}
impl DemoState {
    /// Record the page's visibility from `present`. Opening the page makes
    /// each idle lane's status read due now (nothing refreshed it while
    /// hidden). Returns true when a read became due; the caller requests a
    /// redraw so the next update sends it. A lane with a read in flight is
    /// answered by that reply instead.
    ///
    /// Hiding does not disarm a deadline already merged into the shared
    /// host deadline (a backoff or `VISIBLE_REFRESH`): it fires once as a
    /// no-op wake, then nothing is armed while hidden.
    fn set_visible(&mut self, visible: bool) -> bool {
        let mut due = false;
        if visible && !self.visible && self.connected {
            for lane in [&mut self.background, &mut self.capture] {
                if lane.pending.is_none() {
                    lane.refresh = Some(Duration::ZERO);
                    due = true;
                }
            }
        }
        self.visible = visible;
        due
    }
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
            ("bg-showcase", "background.status"),
            self.visible,
            deadline,
        );
        self.capture.tick(
            bridge,
            now,
            &mut self.next,
            ("capture", "capture.status"),
            self.visible,
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
    (mut state, mut redraw): (ResMut<DemoState>, MessageWriter<bevy::window::RequestRedraw>),
    mut focus: ResMut<InputFocus>,
    mut buttons: Query<(Entity, &Action, &mut TabIndex, Has<InteractionDisabled>)>,
    mut labels: Query<(&ActionLabel, &mut Text), Without<Feedback>>,
    mut feedback: Query<(&Feedback, &mut Text), Without<ActionLabel>>,
) {
    if state.visible != visible(&frame) && state.set_visible(visible(&frame)) {
        // Captured in `Last` this update: an immediate re-update sends it.
        redraw.write(bevy::window::RequestRedraw);
    }
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
    fn ok(id: u64, value: Value) -> BusBridgeEvent {
        BusBridgeEvent::Reply {
            request_id: id,
            result: Ok(BusReply {
                rc: 0,
                body: value.to_string(),
                result: None,
            }),
        }
    }

    #[test]
    fn status_is_not_polled_while_hidden_and_only_slowly_while_visible() {
        let (bridge, peer) = test_bridge("shell");
        let mut state = DemoState::default();
        state.event(
            &BusBridgeEvent::Connection {
                state: BusConnectionState::Connected,
                generation: 1,
            },
            Duration::ZERO,
        );
        // The bootstrap read is owed even with the page hidden.
        state.tick(&bridge, Duration::ZERO, &mut LayerHostDeadline::default());
        for call in peer.drain_calls() {
            state.event(
                &ok(call.request_id, json!({"scene":"boing","phase":"idle"})),
                Duration::ZERO,
            );
        }
        for secs in [5, 10, 60] {
            let mut deadline = LayerHostDeadline::default();
            state.tick(&bridge, Duration::from_secs(secs), &mut deadline);
            assert!(peer.drain_calls().is_empty(), "hidden poll at {secs}s");
            assert_eq!(deadline.0, None, "hidden wake at {secs}s");
        }
        // Opening the page makes both lanes due now (the caller redraws).
        let mut deadline = LayerHostDeadline::default();
        assert!(state.set_visible(true));
        assert_eq!(state.capture.refresh, Some(Duration::ZERO));
        let now = Duration::from_secs(61);
        state.tick(&bridge, now, &mut deadline);
        let calls = peer.drain_calls();
        assert_eq!(calls.len(), 2);
        for call in calls {
            state.event(&ok(call.request_id, json!({"scene":"boing"})), now);
        }
        assert_eq!(state.background.refresh, Some(now + VISIBLE_REFRESH));
        state.tick(
            &bridge,
            now + VISIBLE_REFRESH - Duration::from_millis(1),
            &mut LayerHostDeadline::default(),
        );
        assert!(peer.drain_calls().is_empty(), "no faster than VISIBLE_REFRESH");
        state.tick(&bridge, now + VISIBLE_REFRESH, &mut LayerHostDeadline::default());
        assert_eq!(peer.drain_calls().len(), 2);
    }

    #[test]
    fn failed_status_backs_off_to_a_cap_and_only_retries_while_visible() {
        let (bridge, peer) = test_bridge("shell");
        let mut lane = Lane::default();
        let mut id = 0;
        let mut now = Duration::ZERO;
        let mut gaps = Vec::new();
        let tick = |lane: &mut Lane, id: &mut u64, now: Duration, visible: bool| {
            let mut deadline = LayerHostDeadline::default();
            lane.tick(&bridge, now, id, ("capture", "capture.status"), visible, &mut deadline);
            deadline.0
        };
        for _ in 0..7 {
            tick(&mut lane, &mut id, now, true);
            let call = peer.drain_calls().remove(0);
            lane.event(
                &BusBridgeEvent::Reply {
                    request_id: call.request_id,
                    result: Err("capture is down".into()),
                },
                now,
            );
            let at = lane.refresh.expect("a failure schedules a retry");
            tick(&mut lane, &mut id, at - Duration::from_millis(1), true);
            assert!(peer.drain_calls().is_empty());
            gaps.push((at - now).as_secs());
            now = at;
        }
        assert_eq!(gaps, [2, 4, 8, 16, 30, 30, 30]);
        assert_eq!(tick(&mut lane, &mut id, now, false), None);
        assert!(peer.drain_calls().is_empty(), "hidden page: no retry");
        tick(&mut lane, &mut id, now, true);
        let call = peer.drain_calls().remove(0);
        lane.event(&ok(call.request_id, json!({"phase":"idle"})), now);
        assert_eq!(lane.backoff, RETRY_INITIAL, "success resets the backoff");
    }

    #[test]
    fn a_hidden_action_is_still_read_back_and_other_deadlines_survive() {
        let (bridge, peer) = test_bridge("shell");
        let mut state = DemoState::default();
        state.event(
            &BusBridgeEvent::Connection {
                state: BusConnectionState::Connected,
                generation: 1,
            },
            Duration::ZERO,
        );
        state.tick(&bridge, Duration::ZERO, &mut LayerHostDeadline::default());
        for call in peer.drain_calls() {
            state.event(&ok(call.request_id, json!({"scene":"boing"})), Duration::ZERO);
        }
        // Another projection's wake survives a hidden tick with reads due.
        let holders = Some(Duration::from_secs(7));
        let mut deadline = LayerHostDeadline(holders);
        state.tick(&bridge, VISIBLE_REFRESH * 4, &mut deadline);
        assert!(peer.drain_calls().is_empty());
        assert_eq!(deadline.0, holders);
        // The page is hidden right after the click: the ack still owes a
        // status readback, sent without the page.
        state.background.queue("boing.kick", json!({}));
        let now = VISIBLE_REFRESH * 4;
        state.tick(&bridge, now, &mut LayerHostDeadline::default());
        let kick = peer.drain_calls().remove(0);
        assert_eq!(kick.command, "boing.kick");
        state.event(&ok(kick.request_id, json!({"accepted":true})), now);
        state.tick(&bridge, now, &mut LayerHostDeadline::default());
        assert_eq!(peer.drain_calls()[0].command, "background.status");
    }

    #[test]
    fn a_timed_out_status_backs_off_and_a_hidden_one_is_not_retried() {
        let (bridge, peer) = test_bridge("shell");
        let mut id = 0;
        let mut tick = |lane: &mut Lane, now: Duration, visible: bool| {
            let mut deadline = LayerHostDeadline::default();
            lane.tick(&bridge, now, &mut id, ("capture", "capture.status"), visible, &mut deadline);
            deadline.0
        };
        for visible in [true, false] {
            let mut lane = Lane::default();
            tick(&mut lane, Duration::ZERO, visible);
            assert_eq!(peer.drain_calls().len(), 1);
            let timeout = Duration::from_secs(3);
            tick(&mut lane, timeout, visible);
            assert!(peer.drain_calls().is_empty(), "no flat 3 s retry");
            assert_eq!(lane.refresh, Some(timeout + RETRY_INITIAL));
            let armed = tick(&mut lane, timeout + RETRY_INITIAL, visible);
            if visible {
                assert_eq!(peer.drain_calls().len(), 1, "visible: backed-off retry");
            } else {
                assert!(peer.drain_calls().is_empty(), "hidden: no retry");
                assert_eq!(armed, None, "hidden: no retry wake");
            }
        }
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
