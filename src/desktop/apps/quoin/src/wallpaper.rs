//! Background preferences over Quoin's existing, single-owner Bus bridge.
use std::{collections::BTreeMap, time::Duration};

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
    bus::{BusBridge, BusBridgeEvent, BusConnectionState, BusMessage},
    theme::tokens,
};
use serde_json::{Value, json};

const FIELDS: [(&str, &str); 9] = [
    ("enabled", "Enabled"),
    ("paused", "Paused"),
    ("preset", "Palette"),
    ("flock.count", "Birds"),
    ("speed", "Speed"),
    ("pointer.radius", "Pointer radius"),
    ("window.margin", "Window margin"),
    ("fps_limit", "Frame limit"),
    ("seed", "Seed"),
];

#[derive(Clone, Copy)]
enum RequestKind {
    Get,
    Set,
}

#[derive(Resource)]
pub(crate) struct WallpaperState {
    generation: Option<u64>,
    next_id: u64,
    pending: Option<(u64, RequestKind, Duration)>,
    queued: Option<(&'static str, Value)>,
    refresh: Duration,
    settings: Option<Value>,
    feedback: String,
}

impl Default for WallpaperState {
    fn default() -> Self {
        Self {
            generation: None,
            next_id: 0x57_0000_0000,
            pending: None,
            queued: None,
            refresh: Duration::ZERO,
            settings: None,
            feedback: String::new(),
        }
    }
}

fn leaf<'a>(settings: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.')
        .try_fold(settings, |value, part| value.get(part))
}

fn valid_settings(value: &Value) -> bool {
    ["enabled", "paused"]
        .iter()
        .all(|p| leaf(value, p).is_some_and(Value::is_boolean))
        && matches!(
            leaf(value, "preset").and_then(Value::as_str),
            Some("ocean" | "ember" | "twilight")
        )
        && leaf(value, "seed").and_then(Value::as_u64).is_some()
        && leaf(value, "flock.count")
            .and_then(Value::as_u64)
            .is_some_and(|n| n <= 1024)
        && leaf(value, "fps_limit")
            .and_then(Value::as_u64)
            .is_some_and(|n| (1..=60).contains(&n))
        && [
            ("speed", 5.0, 300.0),
            ("pointer.radius", 0.0, 600.0),
            ("window.margin", 0.0, 100.0),
        ]
        .iter()
        .all(|(p, min, max)| {
            leaf(value, p)
                .and_then(Value::as_f64)
                .is_some_and(|n| n.is_finite() && (*min..=*max).contains(&n))
        })
}

impl WallpaperState {
    fn reset(&mut self) {
        self.settings = None;
        self.pending = None;
        self.queued = None;
        self.refresh = Duration::ZERO;
    }

    pub(crate) fn event(&mut self, event: &BusBridgeEvent, now: Duration) {
        match event {
            BusBridgeEvent::Connection {
                state: BusConnectionState::Connected,
                generation,
            } => {
                self.reset();
                self.generation = Some(*generation);
                self.feedback.clear();
            }
            BusBridgeEvent::Connection { .. } | BusBridgeEvent::Fatal(_) => {
                self.reset();
                self.generation = None;
            }
            BusBridgeEvent::DroppedMessages(_) => {
                self.settings = None;
                self.refresh = now;
            }
            BusBridgeEvent::Reply { request_id, result } => {
                let Some((id, kind, _)) = self.pending else {
                    return;
                };
                if id != *request_id {
                    return;
                }
                self.pending = None;
                self.refresh = now;
                match result {
                    Ok(reply) if reply.rc == 0 => {
                        let value = serde_json::from_str::<Value>(&reply.body).ok();
                        match kind {
                            RequestKind::Get => {
                                self.settings = value.filter(valid_settings);
                                if self.settings.is_none() {
                                    self.feedback = "Background unavailable".into();
                                } else if self.feedback == "Background unavailable" {
                                    self.feedback.clear();
                                }
                                self.refresh = now + Duration::from_secs(1);
                            }
                            RequestKind::Set => {
                                self.feedback =
                                    if value.as_ref().is_some_and(|v| v["persisted"] == true) {
                                        "Saved".into()
                                    } else {
                                        "Save unconfirmed; checking settings".into()
                                    };
                                self.settings = None;
                            }
                        }
                    }
                    _ => {
                        self.settings = None;
                        self.feedback = match kind {
                            RequestKind::Get => "Background unavailable".into(),
                            RequestKind::Set => "Could not confirm save; checking settings".into(),
                        };
                        self.refresh = now + Duration::from_secs(1);
                    }
                }
            }
            _ => {}
        }
    }

    pub(crate) fn message(&mut self, message: &BusMessage, now: Duration) {
        // Notifications only invalidate. No untrusted event body becomes UI
        // state; the next correlated service reply supplies the full tree.
        if Some(message.connection_generation) == self.generation
            && matches!(
                message.topic(),
                Some("wallpaper.props.changed" | "bg-showcase.props.changed")
            )
        {
            self.refresh = now;
        }
    }

    pub(crate) fn tick(
        &mut self,
        bridge: &BusBridge,
        now: Duration,
        deadline: &mut LayerHostDeadline,
    ) {
        if bridge.worker_is_gone() {
            self.reset();
            self.generation = None;
        }
        if self.generation.is_none() {
            deadline.0 = None;
            return;
        }
        if self.pending.is_some_and(|(_, _, at)| at <= now) {
            self.pending = None;
            self.settings = None;
            self.feedback = "Request timed out; checking settings".into();
            self.refresh = now;
        }
        if self.pending.is_none() && self.refresh <= now {
            self.next_id = self
                .next_id
                .checked_add(1)
                .expect("wallpaper request ID exhausted");
            let (kind, command, body) = match self.queued.as_ref() {
                Some((path, value)) => (
                    RequestKind::Set,
                    "wallpaper.props.set",
                    json!({"path":path,"value":value}),
                ),
                None => (RequestKind::Get, "wallpaper.props.get", json!({})),
            };
            if bridge
                .try_call(
                    self.next_id,
                    std::env::var("COSMIX_QUOIN_BACKGROUND_SERVICE")
                        .unwrap_or_else(|_| "wallpaper".into()),
                    command,
                    BTreeMap::new(),
                    body.to_string(),
                )
                .is_ok()
            {
                self.queued = None;
                self.pending = Some((self.next_id, kind, now + Duration::from_secs(3)));
            } else {
                self.refresh = now + Duration::from_millis(100);
            }
        }
        deadline.0 = Some(self.pending.map_or(self.refresh, |(_, _, at)| at));
    }

    fn available(&self) -> bool {
        self.generation.is_some()
            && self.settings.is_some()
            && self.pending.is_none()
            && self.queued.is_none()
    }
}

pub(crate) struct WallpaperPlugin;
impl Plugin for WallpaperPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(activate)
            .add_systems(Update, present.in_set(ShellRuntimeSet::Host));
    }
}

#[derive(Component)]
struct SettingButton(usize);
#[derive(Component)]
struct SettingLabel(usize);
#[derive(Component)]
struct Feedback;

pub(crate) fn controls(commands: &mut Commands) -> Entity {
    let root = commands
        .spawn(Node {
            flex_direction: FlexDirection::Column,
            row_gap: px(4),
            ..default()
        })
        .id();
    let title = commands
        .spawn((
            Text::new("Background · click a value to change it"),
            bevy::feathers::theme::ThemeTextColor(tokens::TEXT),
            TextFont {
                font_size: FontSize::Px(12.0),
                ..default()
            },
        ))
        .id();
    commands.entity(root).add_child(title);
    for (index, (_, label)) in FIELDS.iter().enumerate() {
        let button = commands
            .spawn((
                Node {
                    min_height: px(25),
                    padding: UiRect::axes(px(8), px(3)),
                    ..default()
                },
                bevy::feathers::theme::ThemeBackgroundColor(tokens::CONTROL),
                WidgetButton,
                Pickable::default(),
                Hovered::default(),
                TabIndex(-1),
                InteractionDisabled,
                SettingButton(index),
            ))
            .id();
        let text = commands
            .spawn((
                SettingLabel(index),
                Text::new(format!("{label}: —")),
                bevy::feathers::theme::ThemeTextColor(tokens::TEXT),
                TextFont {
                    font_size: FontSize::Px(13.0),
                    ..default()
                },
            ))
            .id();
        commands.entity(button).add_child(text);
        commands.entity(root).add_child(button);
    }
    let feedback = commands
        .spawn((
            Feedback,
            Text::new("Connecting to background…"),
            bevy::feathers::theme::ThemeTextColor(tokens::TEXT),
            TextFont {
                font_size: FontSize::Px(12.0),
                ..default()
            },
        ))
        .id();
    commands.entity(root).add_child(feedback);
    root
}

fn visible(frame: &ShellFrameState) -> bool {
    let panel = frame.0.panel(Edge::Right);
    panel.mapped && panel.active_page_id.as_deref() == Some("monitor")
}

fn next_value(index: usize, value: &Value) -> Value {
    match index {
        0 | 1 => json!(!value.as_bool().unwrap_or(false)),
        2 => json!(match value.as_str() {
            Some("ocean") => "ember",
            Some("ember") => "twilight",
            _ => "ocean",
        }),
        8 => json!(value.as_u64().unwrap_or(0).checked_add(1).unwrap_or(0)),
        _ => {
            let values: &[u64] = match index {
                3 => &[64, 128, 192, 384, 768, 1024],
                4 => &[40, 80, 120, 180],
                5 => &[0, 60, 110, 180],
                6 => &[0, 8, 12, 24, 48],
                _ => &[15, 24, 30, 60],
            };
            let current = value.as_f64().unwrap_or(0.0);
            json!(
                values
                    .iter()
                    .find(|n| **n as f64 > current)
                    .copied()
                    .unwrap_or(values[0])
            )
        }
    }
}

fn activate(
    event: On<Activate>,
    buttons: Query<(&SettingButton, Has<InteractionDisabled>)>,
    frame: Res<ShellFrameState>,
    mut state: ResMut<WallpaperState>,
    mut redraw: MessageWriter<bevy::window::RequestRedraw>,
) {
    let Ok((button, disabled)) = buttons.get(event.entity) else {
        return;
    };
    if disabled || !visible(&frame) || !state.available() {
        return;
    }
    let path = FIELDS[button.0].0;
    let value = leaf(state.settings.as_ref().unwrap(), path).unwrap();
    state.queued = Some((path, next_value(button.0, value)));
    state.refresh = Duration::ZERO;
    state.feedback = "Saving…".into();
    redraw.write(bevy::window::RequestRedraw);
}

fn present(
    mut commands: Commands,
    frame: Res<ShellFrameState>,
    state: Res<WallpaperState>,
    mut focus: ResMut<InputFocus>,
    mut buttons: Query<(Entity, &mut TabIndex, Has<InteractionDisabled>), With<SettingButton>>,
    mut labels: Query<(&SettingLabel, &mut Text), Without<Feedback>>,
    mut feedback: Query<&mut Text, (With<Feedback>, Without<SettingLabel>)>,
) {
    let enabled = visible(&frame) && state.available();
    let keep_focus = visible(&frame)
        && state.generation.is_some()
        && (state.settings.is_some() || state.pending.is_some());
    for (entity, mut tab, disabled) in &mut buttons {
        let wanted_tab = if enabled { 0 } else { -1 };
        if tab.0 != wanted_tab {
            tab.0 = wanted_tab;
        }
        if enabled && disabled {
            commands.entity(entity).remove::<InteractionDisabled>();
        }
        if !enabled && !disabled {
            commands.entity(entity).insert(InteractionDisabled);
        }
        if !keep_focus && focus.get() == Some(entity) {
            focus.clear();
        }
    }
    for (label, mut text) in &mut labels {
        let (path, name) = FIELDS[label.0];
        let value = state
            .settings
            .as_ref()
            .and_then(|s| leaf(s, path))
            .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
            .unwrap_or_else(|| "—".into());
        let next = format!("{name}: {value}");
        if text.0 != next {
            text.0 = next;
        }
    }
    for mut text in &mut feedback {
        let next = if state.settings.is_none() && state.pending.is_some() {
            "Checking background…".into()
        } else if state.generation.is_none() {
            "Background unavailable".into()
        } else {
            state.feedback.clone()
        };
        if text.0 != next {
            text.0 = next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ctk::bus::{BusReply, test_bridge};

    fn settings() -> Value {
        json!({"enabled":true,"paused":false,"preset":"ocean","flock":{"count":192},
            "speed":80,"pointer":{"radius":110},"window":{"margin":12},"fps_limit":30,"seed":42})
    }
    fn reply(id: u64, value: Value) -> BusBridgeEvent {
        BusBridgeEvent::Reply {
            request_id: id,
            result: Ok(BusReply {
                rc: 0,
                body: value.to_string(),
                result: None,
            }),
        }
    }
    fn connected(state: &mut WallpaperState, generation: u64) {
        state.event(
            &BusBridgeEvent::Connection {
                state: BusConnectionState::Connected,
                generation,
            },
            Duration::ZERO,
        );
    }

    #[test]
    fn writes_wait_for_ack_and_authoritative_readback_then_reconcile() {
        let (bridge, peer) = test_bridge("shell");
        let mut state = WallpaperState::default();
        let mut deadline = LayerHostDeadline::default();
        connected(&mut state, 1);
        state.tick(&bridge, Duration::ZERO, &mut deadline);
        let get = peer.drain_calls().remove(0);
        assert_eq!(get.command, "wallpaper.props.get");
        assert!(!state.available());
        state.event(&reply(get.request_id, settings()), Duration::ZERO);
        assert!(state.available());
        state.queued = Some(("paused", json!(true)));
        state.refresh = Duration::ZERO;
        state.tick(&bridge, Duration::ZERO, &mut deadline);
        let set = peer.drain_calls().remove(0);
        assert_eq!(set.command, "wallpaper.props.set");
        assert_eq!(
            serde_json::from_str::<Value>(&set.body).unwrap(),
            json!({"path":"paused","value":true})
        );
        assert_eq!(
            leaf(state.settings.as_ref().unwrap(), "paused"),
            Some(&json!(false))
        );
        assert!(!state.available());
        state.event(
            &reply(set.request_id, json!({"persisted":true})),
            Duration::ZERO,
        );
        assert!(!state.available());
        state.tick(&bridge, Duration::ZERO, &mut deadline);
        let get = peer.drain_calls().remove(0);
        let mut changed = settings();
        changed["paused"] = json!(true);
        state.event(&reply(get.request_id, changed), Duration::ZERO);
        assert!(state.available());
        state.tick(&bridge, Duration::from_secs(1), &mut deadline);
        assert_eq!(
            peer.drain_calls().len(),
            1,
            "lost notifications reconcile on a host deadline"
        );
    }

    #[test]
    fn timeout_and_reconnect_reject_old_replies_and_bound_pending_work() {
        let (bridge, peer) = test_bridge("shell");
        let mut state = WallpaperState::default();
        let mut deadline = LayerHostDeadline::default();
        connected(&mut state, 1);
        state.tick(&bridge, Duration::ZERO, &mut deadline);
        let old = peer.drain_calls().remove(0);
        for _ in 0..10 {
            state.tick(&bridge, Duration::from_secs(1), &mut deadline);
        }
        assert!(peer.drain_calls().is_empty());
        assert_eq!(deadline.0, Some(Duration::from_secs(3)));
        state.tick(&bridge, Duration::from_secs(3), &mut deadline);
        let retry = peer.drain_calls().remove(0);
        state.event(&reply(old.request_id, settings()), Duration::from_secs(3));
        assert!(state.settings.is_none());
        connected(&mut state, 2);
        state.tick(&bridge, Duration::from_secs(3), &mut deadline);
        let current = peer.drain_calls().remove(0);
        state.event(&reply(retry.request_id, settings()), Duration::from_secs(3));
        assert!(state.settings.is_none());
        state.event(
            &reply(current.request_id, settings()),
            Duration::from_secs(3),
        );
        assert!(state.available());
        drop(peer);
        state.tick(&bridge, Duration::from_secs(4), &mut deadline);
        assert!(!state.available());
        assert_eq!(deadline.0, None);
    }

    #[test]
    fn all_controls_have_typed_choices_and_incomplete_snapshots_stay_disabled() {
        let tree = settings();
        assert!(valid_settings(&tree));
        assert!(!valid_settings(&json!({"enabled":true})));
        for (index, (path, _)) in FIELDS.iter().enumerate() {
            let value = leaf(&tree, path).unwrap();
            let changed = next_value(index, value);
            assert_ne!(&changed, value, "{path}");
            assert_eq!(changed.is_boolean(), value.is_boolean());
            assert_eq!(changed.is_string(), value.is_string());
        }
        assert_eq!(next_value(8, &json!(u64::MAX)), json!(0));
    }

    #[test]
    fn real_buttons_require_visible_page_and_clear_focus_when_hidden() {
        use bevy::ecs::system::RunSystemOnce;
        use cosmix_shell::{
            core::{LogicalSize, OutputKey, ShellModel},
            runtime::ShellFrame,
        };
        let model = ShellModel::new(
            OutputKey::new("test").unwrap(),
            LogicalSize::new(1536.0, 864.0).unwrap(),
            Duration::ZERO,
            Duration::from_millis(800),
            Duration::from_millis(200),
        )
        .unwrap();
        let mut app = App::new();
        let mut state = WallpaperState::default();
        connected(&mut state, 1);
        state.settings = Some(settings());
        app.insert_resource(state)
            .insert_resource(ShellFrameState(ShellFrame::from_model(&model)))
            .init_resource::<InputFocus>()
            .add_message::<bevy::window::RequestRedraw>()
            .add_observer(activate);
        let mut queue = bevy::ecs::world::CommandQueue::default();
        controls(&mut Commands::new(&mut queue, app.world()));
        queue.apply(app.world_mut());
        let buttons: Vec<_> = app
            .world_mut()
            .query::<(Entity, &SettingButton)>()
            .iter(app.world())
            .map(|(e, b)| (e, b.0))
            .collect();
        assert_eq!(buttons.len(), FIELDS.len());
        app.world_mut().run_system_once(present).unwrap();
        for (entity, _) in &buttons {
            assert!(app.world().entity(*entity).contains::<WidgetButton>());
            app.world_mut().trigger(Activate { entity: *entity });
            assert!(app.world().resource::<WallpaperState>().queued.is_none());
        }
        {
            let mut frame = app.world_mut().resource_mut::<ShellFrameState>();
            let panel = &mut frame.0.panels[Edge::Right.index()];
            panel.mapped = true;
            panel.active_page_id = Some("monitor".into());
        }
        app.world_mut().run_system_once(present).unwrap();
        let (entity, index) = buttons[0];
        app.world_mut()
            .resource_mut::<InputFocus>()
            .set(entity, bevy::input_focus::FocusCause::Navigated);
        app.world_mut().resource_mut::<WallpaperState>().pending =
            Some((123, RequestKind::Get, Duration::from_secs(3)));
        app.world_mut().run_system_once(present).unwrap();
        assert_eq!(
            app.world().resource::<InputFocus>().get(),
            Some(entity),
            "background refresh must preserve keyboard focus"
        );
        app.world_mut().trigger(Activate { entity });
        assert!(
            app.world().resource::<WallpaperState>().queued.is_none(),
            "refresh still blocks activation"
        );
        app.world_mut().resource_mut::<WallpaperState>().pending = None;
        app.world_mut().run_system_once(present).unwrap();
        assert_eq!(app.world().resource::<InputFocus>().get(), Some(entity));
        app.world_mut().trigger(Activate { entity });
        assert_eq!(
            app.world()
                .resource::<WallpaperState>()
                .queued
                .as_ref()
                .unwrap()
                .0,
            FIELDS[index].0
        );
        let original = app.world().resource::<WallpaperState>().queued.clone();
        app.world_mut().trigger(Activate {
            entity: buttons[1].0,
        });
        assert_eq!(
            app.world().resource::<WallpaperState>().queued,
            original,
            "only one accepted action can be pending"
        );
        app.world_mut()
            .resource_mut::<InputFocus>()
            .set(entity, bevy::input_focus::FocusCause::Navigated);
        app.world_mut().resource_mut::<ShellFrameState>().0.panels[Edge::Right.index()]
            .active_page_id = Some("agents".into());
        app.world_mut().run_system_once(present).unwrap();
        assert_eq!(app.world().resource::<InputFocus>().get(), None);
        assert_eq!(app.world().entity(entity).get::<TabIndex>().unwrap().0, -1);
        app.world_mut().resource_mut::<WallpaperState>().queued = None;
        app.world_mut()
            .entity_mut(entity)
            .remove::<InteractionDisabled>();
        app.world_mut().trigger(Activate { entity });
        assert!(
            app.world().resource::<WallpaperState>().queued.is_none(),
            "stale enabled components cannot bypass page visibility"
        );
    }
}
