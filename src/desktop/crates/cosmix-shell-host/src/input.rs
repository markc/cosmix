//! Native SCTK pointer, keyboard and touch input translated into Bevy input.

use std::collections::BTreeMap;
use std::time::Duration;

use bevy::app::{PreUpdate, Update};
use bevy::ecs::message::{MessageReader, MessageWriter};
use bevy::input::keyboard::{Key, KeyCode, KeyboardFocusLost, KeyboardInput, NativeKey};
use bevy::input::mouse::{MouseButton, MouseButtonInput, MouseScrollUnit, MouseWheel};
use bevy::input::touch::{TouchInput, TouchPhase};
use bevy::input::{ButtonState, InputSystems};
use bevy::picking::events::PointerState;
use bevy::picking::pointer::{
    PointerAction, PointerId, PointerInput, PointerLocation, PointerPress,
};
use bevy::prelude::{
    App, Entity, IntoScheduleConfigs, Res, ResMut, Resource, Time, Vec2, Window, World,
};
use bevy::time::Real;
use bevy::window::{CursorEntered, CursorLeft, CursorMoved, WindowEvent, WindowFocused};
use bevy_winit::converters::{convert_logical_key, convert_physical_key_code};
use cosmix_shell::core::{CornerEvent, Edge, OutputKey, PanelInput};
use cosmix_shell::runtime::{ShellCommand, ShellCommandKind, ShellRuntimeSet};
use smithay_client_toolkit::seat::keyboard::{
    KeyEvent as SctkKeyEvent, Keymap as SctkKeymap, Keysym, Modifiers, RepeatInfo,
};
use smithay_client_toolkit::seat::pointer::{
    AxisScroll, BTN_BACK, BTN_EXTRA, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, BTN_SIDE,
    PointerEvent, PointerEventKind,
};
use wayland_client::Proxy;
use wayland_client::protocol::wl_surface;
use winit::keyboard::PhysicalKey;
use winit::platform::scancode::PhysicalKeyExtScancode;
use xkbcommon::xkb;

#[derive(Clone)]
pub(crate) struct SurfaceTarget {
    pub surface: wl_surface::WlSurface,
    pub window: Entity,
    pub edge: Edge,
    pub output_size: Vec2,
    pub thickness: f32,
    pub committed_margin: i32,
}

#[derive(Clone, Debug)]
struct MappedKey {
    raw_code: u32,
    key_code: KeyCode,
    logical_key: Key,
    text: Option<String>,
}

#[derive(Clone, Debug)]
struct KeyboardFocus {
    surface: Option<wl_surface::WlSurface>,
    window: Entity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RepeatSettings {
    delay: Duration,
    gap: Duration,
}

const MIN_REPEAT_DELAY: Duration = Duration::from_millis(50);
const MAX_REPEAT_DELAY: Duration = Duration::from_secs(2);
const MIN_REPEAT_GAP: Duration = Duration::from_millis(8);
const MAX_REPEAT_GAP: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct KeymapUpdate {
    pub installed: bool,
    pub deadline_changed: bool,
}

#[derive(Clone, Debug)]
struct RepeatingKey {
    key: MappedKey,
    next_deadline: Duration,
}

/// One active SCTK keyboard translated into Bevy's renderer-neutral input.
/// Repeat is intentionally data-only here: the runner folds its deadline into
/// the host's single replaceable calloop timer.
#[derive(Default)]
pub(crate) struct KeyboardBridge {
    focus: Option<KeyboardFocus>,
    pressed: BTreeMap<u32, MappedKey>,
    repeat_settings: Option<RepeatSettings>,
    repeating: Option<RepeatingKey>,
    keymap: Option<xkb::Keymap>,
    keymap_source: Option<String>,
    diagnostics: u64,
}

impl KeyboardBridge {
    pub(crate) fn install_keymap(&mut self, keymap: SctkKeymap<'_>) -> KeymapUpdate {
        self.install_keymap_text(keymap.as_string())
    }

    fn install_keymap_text(&mut self, source: String) -> KeymapUpdate {
        if self.keymap_source.as_ref() == Some(&source) {
            return KeymapUpdate {
                installed: true,
                deadline_changed: false,
            };
        }
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let Some(keymap) = xkb::Keymap::new_from_string(
            &context,
            source.clone(),
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::COMPILE_NO_FLAGS,
        ) else {
            self.reject("invalid-keymap-copy");
            return KeymapUpdate {
                installed: false,
                deadline_changed: self.cancel_repeat(),
            };
        };
        let deadline_changed = self.cancel_repeat();
        self.keymap = Some(keymap);
        self.keymap_source = Some(source);
        KeymapUpdate {
            installed: true,
            deadline_changed,
        }
    }

    pub(crate) fn enter(
        &mut self,
        app: &mut App,
        target: &SurfaceTarget,
        raw: &[u32],
        keysyms: &[Keysym],
    ) -> bool {
        if self.focus.is_some() {
            self.focus_lost(app);
        }
        self.focus = Some(KeyboardFocus {
            surface: Some(target.surface.clone()),
            window: target.window,
        });
        set_window_focused(app, target.window, true);
        emit_window(
            app,
            WindowFocused {
                window: target.window,
                focused: true,
            },
        );
        // SCTK 0.19.2 constructs `keysyms` one-for-one from `raw`, including
        // NoSymbol placeholders. Surface a changed upstream invariant in debug
        // builds instead of silently dropping an enter-held key from the zip.
        // Revisit this guard whenever SCTK is upgraded.
        debug_assert_eq!(raw.len(), keysyms.len());
        for (&raw_code, &keysym) in raw.iter().zip(keysyms) {
            let mapped = map_key(raw_code, keysym, None);
            emit_keyboard(app, target.window, &mapped, ButtonState::Pressed, false);
            self.pressed.insert(raw_code, mapped);
        }
        true
    }

    pub(crate) fn leave(&mut self, app: &mut App, surface: &wl_surface::WlSurface) -> bool {
        if self
            .focus
            .as_ref()
            .is_none_or(|focus| focus.surface.as_ref() != Some(surface))
        {
            return false;
        }
        self.focus_lost(app)
    }

    pub(crate) fn press(&mut self, app: &mut App, event: SctkKeyEvent, elapsed: Duration) -> bool {
        let Some(window) = self.focus.as_ref().map(|focus| focus.window) else {
            self.reject("key-without-focus");
            return false;
        };
        let mapped = map_key(event.raw_code, event.keysym, event.utf8);
        emit_keyboard(app, window, &mapped, ButtonState::Pressed, false);
        self.pressed.insert(event.raw_code, mapped.clone());
        if self.key_repeats(event.raw_code)
            && let Some(settings) = self.repeat_settings
        {
            self.cancel_repeat();
            self.repeating = Some(RepeatingKey {
                key: mapped,
                next_deadline: elapsed.saturating_add(settings.delay),
            });
        }
        true
    }

    pub(crate) fn release(&mut self, app: &mut App, event: SctkKeyEvent) -> bool {
        let Some(window) = self.focus.as_ref().map(|focus| focus.window) else {
            return false;
        };
        let mapped = self
            .pressed
            .remove(&event.raw_code)
            .unwrap_or_else(|| map_key(event.raw_code, event.keysym, None));
        emit_keyboard(app, window, &mapped, ButtonState::Released, false);
        if self
            .repeating
            .as_ref()
            .is_some_and(|repeat| repeat.key.raw_code == event.raw_code)
        {
            self.cancel_repeat();
        }
        true
    }

    pub(crate) fn update_modifiers(&mut self, _modifiers: Modifiers, _layout: u32) -> bool {
        // Physical modifier keys already enter Bevy through press/release.
        // SCTK deliberately exposes only an effective six-boolean summary
        // here, not the raw XKB masks needed to reinterpret text losslessly.
        // Invariant: every modifier callback cancels repeat because no callback
        // is provably text-irrelevant without those raw masks or live XKB state.
        self.cancel_repeat()
    }

    pub(crate) fn update_repeat_info(&mut self, info: RepeatInfo, elapsed: Duration) -> bool {
        let previous_deadline = self.repeat_deadline();
        self.repeat_settings = match info {
            RepeatInfo::Disable => None,
            RepeatInfo::Repeat { rate, delay } => Some(RepeatSettings {
                // The compositor controls these values. Bound them to human
                // input rates so repeat can never starve the calloop dispatch.
                delay: Duration::from_millis(u64::from(delay))
                    .clamp(MIN_REPEAT_DELAY, MAX_REPEAT_DELAY),
                gap: Duration::from_micros(1_000_000 / u64::from(rate.get()))
                    .clamp(MIN_REPEAT_GAP, MAX_REPEAT_GAP),
            }),
        };
        let Some(settings) = self.repeat_settings else {
            self.cancel_repeat();
            return previous_deadline != self.repeat_deadline();
        };
        if let Some(repeating) = self.repeating.as_mut() {
            repeating.next_deadline = elapsed.saturating_add(settings.delay);
        }
        previous_deadline != self.repeat_deadline()
    }

    pub(crate) fn repeat_deadline(&self) -> Option<Duration> {
        self.repeating.as_ref().map(|repeat| repeat.next_deadline)
    }

    #[cfg(test)]
    pub(crate) fn repeating_for_test(
        window: Entity,
        next_deadline: Duration,
        gap: Duration,
    ) -> Self {
        let key = map_key(30, Keysym::a, Some("a".to_owned()));
        Self {
            focus: Some(KeyboardFocus {
                surface: None,
                window,
            }),
            pressed: BTreeMap::from([(key.raw_code, key.clone())]),
            repeat_settings: Some(RepeatSettings {
                delay: MIN_REPEAT_DELAY,
                gap,
            }),
            repeating: Some(RepeatingKey { key, next_deadline }),
            ..Default::default()
        }
    }

    pub(crate) fn fire_repeat(&mut self, app: &mut App, elapsed: Duration) -> bool {
        let (Some(window), Some(settings)) = (
            self.focus.as_ref().map(|focus| focus.window),
            self.repeat_settings,
        ) else {
            return false;
        };
        let Some(repeating) = self.repeating.as_mut() else {
            return false;
        };
        if elapsed < repeating.next_deadline {
            return false;
        }
        let key = repeating.key.clone();
        let next_deadline = repeating
            .next_deadline
            .max(elapsed)
            .checked_add(settings.gap);
        emit_keyboard(app, window, &key, ButtonState::Pressed, true);
        if let Some(next_deadline) = next_deadline {
            repeating.next_deadline = next_deadline;
        } else {
            self.repeating = None;
        }
        true
    }

    pub(crate) fn cleanup(&mut self, app: &mut App, window: Option<Entity>) -> bool {
        if window.is_some_and(|window| {
            self.focus
                .as_ref()
                .is_none_or(|focus| focus.window != window)
        }) {
            return false;
        }
        self.focus_lost(app)
    }

    fn focus_lost(&mut self, app: &mut App) -> bool {
        let Some(focus) = self.focus.take() else {
            return false;
        };
        for (_, mapped) in std::mem::take(&mut self.pressed) {
            emit_keyboard(app, focus.window, &mapped, ButtonState::Released, false);
        }
        self.cancel_repeat();
        set_window_focused(app, focus.window, false);
        emit_window(
            app,
            WindowFocused {
                window: focus.window,
                focused: false,
            },
        );
        true
    }

    fn key_repeats(&self, raw_code: u32) -> bool {
        self.keymap
            .as_ref()
            .is_some_and(|keymap| keymap.key_repeats(xkb::Keycode::new(raw_code.saturating_add(8))))
    }

    fn cancel_repeat(&mut self) -> bool {
        self.repeating.take().is_some()
    }

    fn reject(&mut self, reason: &'static str) {
        self.diagnostics = self.diagnostics.saturating_add(1);
        if self.diagnostics.is_power_of_two() {
            tracing::warn!(
                event = "quoin_keyboard_event_rejected",
                count = self.diagnostics,
                reason
            );
        }
    }
}

fn map_key(raw_code: u32, keysym: Keysym, text: Option<String>) -> MappedKey {
    let key_code = convert_physical_key_code(PhysicalKey::from_scancode(raw_code));
    MappedKey {
        raw_code,
        key_code,
        logical_key: logical_key(keysym),
        text: text.filter(|value| !value.is_empty()),
    }
}

fn logical_key(keysym: Keysym) -> Key {
    let mapped = crate::input_keysym::keysym_to_key(keysym.raw());
    if !matches!(mapped, winit::keyboard::Key::Unidentified(_)) {
        return convert_logical_key(&mapped);
    }
    if matches!(
        keysym.raw(),
        0xfe50..=0xfe6f | 0xfe80..=0xfe8c | 0xfe90..=0xfe93
    ) {
        let character = char::from_u32(xkb::keysym_to_utf32(keysym)).filter(|value| *value != '\0');
        return Key::Dead(character);
    }
    keysym.key_char().map_or_else(
        || Key::Unidentified(NativeKey::Xkb(keysym.raw())),
        |character| Key::Character(character.to_string().into()),
    )
}

fn set_window_focused(app: &mut App, window: Entity, focused: bool) {
    if let Some(mut window) = app.world_mut().get_mut::<Window>(window) {
        window.focused = focused;
    }
}

fn emit_keyboard(app: &mut App, window: Entity, key: &MappedKey, state: ButtonState, repeat: bool) {
    let event = KeyboardInput {
        key_code: key.key_code,
        logical_key: key.logical_key.clone(),
        state,
        text: (state == ButtonState::Pressed)
            .then_some(key.text.as_deref())
            .flatten()
            .map(Into::into),
        repeat,
        window,
    };
    emit_window(app, event);
}

#[derive(Clone, Debug)]
struct TouchContact {
    window: Entity,
    position: Vec2,
}

/// Active touch contacts, attributed on down so later id-only events cannot
/// drift to a recreated or differently selected panel surface.
#[derive(Default)]
pub(crate) struct TouchBridge {
    contacts: BTreeMap<i32, TouchContact>,
    diagnostics: u64,
}

impl TouchBridge {
    pub(crate) fn down(
        &mut self,
        app: &mut App,
        targets: &[SurfaceTarget],
        surface: &wl_surface::WlSurface,
        id: i32,
        position: (f64, f64),
    ) -> bool {
        let Some(target) = targets.iter().find(|target| target.surface == *surface) else {
            return false;
        };
        let Some(position) = valid_position(app, target.window, position) else {
            self.reject("invalid-down-position");
            return false;
        };
        if let Some(previous) = self.contacts.remove(&id) {
            emit_touch(app, id, &previous, TouchPhase::Canceled);
        }
        self.start_contact(app, target.window, id, position);
        true
    }

    fn start_contact(&mut self, app: &mut App, window: Entity, id: i32, position: Vec2) {
        let contact = TouchContact { window, position };
        emit_touch(app, id, &contact, TouchPhase::Started);
        self.contacts.insert(id, contact);
    }

    pub(crate) fn motion(&mut self, app: &mut App, id: i32, position: (f64, f64)) -> bool {
        let Some(previous) = self.contacts.get(&id) else {
            return false;
        };
        let Some(position) = valid_position(app, previous.window, position) else {
            self.reject("invalid-motion-position");
            return false;
        };
        let Some(contact) = self.contacts.get_mut(&id) else {
            return false;
        };
        contact.position = position;
        emit_touch(app, id, contact, TouchPhase::Moved);
        true
    }

    pub(crate) fn up(&mut self, app: &mut App, id: i32) -> bool {
        let Some(contact) = self.contacts.remove(&id) else {
            return false;
        };
        emit_touch(app, id, &contact, TouchPhase::Ended);
        true
    }

    pub(crate) fn cancel(&mut self, app: &mut App) -> bool {
        let changed = !self.contacts.is_empty();
        for (id, contact) in std::mem::take(&mut self.contacts) {
            emit_touch(app, id, &contact, TouchPhase::Canceled);
        }
        changed
    }

    pub(crate) fn cleanup(&mut self, app: &mut App, window: Option<Entity>) -> bool {
        let ids = self
            .contacts
            .iter()
            .filter_map(|(&id, contact)| {
                window
                    .is_none_or(|window| contact.window == window)
                    .then_some(id)
            })
            .collect::<Vec<_>>();
        for id in &ids {
            if let Some(contact) = self.contacts.remove(id) {
                emit_touch(app, *id, &contact, TouchPhase::Canceled);
            }
        }
        !ids.is_empty()
    }

    fn reject(&mut self, reason: &'static str) {
        self.diagnostics = self.diagnostics.saturating_add(1);
        if self.diagnostics.is_power_of_two() {
            tracing::warn!(
                event = "quoin_touch_event_rejected",
                count = self.diagnostics,
                reason
            );
        }
    }
}

fn emit_touch(app: &mut App, id: i32, contact: &TouchContact, phase: TouchPhase) {
    emit_window(
        app,
        TouchInput {
            phase,
            position: contact.position,
            window: contact.window,
            force: None,
            id: id as u64,
        },
    );
}

#[derive(Clone)]
struct Focus {
    surface_id: u32,
    window: Entity,
    edge: Edge,
    position: Vec2,
    output_position: Vec2,
}

#[derive(Resource, Default)]
pub(crate) struct StagedShellCommands(Vec<(OutputKey, ShellCommandKind)>);

pub(crate) fn configure_ingress(app: &mut App) {
    app.add_message::<KeyboardFocusLost>()
        .add_message::<WindowFocused>()
        .add_message::<WindowEvent>()
        .init_resource::<StagedShellCommands>()
        .add_systems(
            Update,
            flush_staged_shell_commands.in_set(ShellRuntimeSet::Input),
        )
        .add_systems(PreUpdate, coalesce_keyboard_focus_lost.before(InputSystems));
}

/// Emit a global keyboard loss only when an update batch contains no focus
/// gain. This mirrors Bevy's winit host and keeps leave(A)+enter(B) in one
/// Wayland dispatch from clearing B's newly pressed input resources. Running
/// before `InputSystems` makes the leave-triggered update consume the marker in
/// the same schedule, so it cannot survive an idle dispatch or a later gain.
fn coalesce_keyboard_focus_lost(
    mut focused: MessageReader<WindowFocused>,
    mut keyboard_focus_lost: MessageWriter<KeyboardFocusLost>,
    mut window_events: MessageWriter<WindowEvent>,
) {
    let mut lost = false;
    let mut gained = false;
    for event in focused.read() {
        lost |= !event.focused;
        gained |= event.focused;
    }
    if lost && !gained {
        keyboard_focus_lost.write(KeyboardFocusLost);
        window_events.write(WindowEvent::KeyboardFocusLost(KeyboardFocusLost));
    }
}

pub(crate) fn stage_shell_command(app: &mut App, output: OutputKey, kind: ShellCommandKind) {
    stage_shell_command_world(app.world_mut(), output, kind);
}

pub(crate) fn stage_shell_command_world(
    world: &mut World,
    output: OutputKey,
    kind: ShellCommandKind,
) {
    world
        .resource_mut::<StagedShellCommands>()
        .0
        .push((output, kind));
}

pub(crate) fn staged_shell_commands_pending(app: &App) -> bool {
    !app.world().resource::<StagedShellCommands>().0.is_empty()
}

fn shell_command_kind(kind: &ShellCommandKind) -> &'static str {
    match kind {
        ShellCommandKind::Quit => "quit",
        ShellCommandKind::Geometry(_) => "geometry",
        ShellCommandKind::Corner(CornerEvent::Entered { .. }) => "corner-entered",
        ShellCommandKind::Corner(CornerEvent::Left { .. }) => "corner-left",
        ShellCommandKind::Panel { .. } => "panel",
        ShellCommandKind::Carousel { .. } => "carousel",
    }
}

fn flush_staged_shell_commands(
    time: Res<Time<Real>>,
    mut staged: ResMut<StagedShellCommands>,
    mut commands: MessageWriter<ShellCommand>,
) {
    let at = time.elapsed();
    for (output, kind) in staged.0.drain(..) {
        tracing::debug!(
            event = "quoin_staged_command_flushed",
            kind = shell_command_kind(&kind),
            output = output.as_str(),
            stamped_us = at.as_micros()
        );
        commands.write(ShellCommand { output, at, kind });
    }
}

#[derive(Default)]
pub(crate) struct PointerBridge {
    focus: Option<Focus>,
    pressed: Vec<MouseButton>,
    axis_active: bool,
    diagnostics: u64,
    last_output_position: Option<Vec2>,
}

impl PointerBridge {
    pub fn frame(
        &mut self,
        app: &mut App,
        output: &OutputKey,
        targets: &[SurfaceTarget],
        events: &[PointerEvent],
    ) -> bool {
        let mut handled = false;
        for event in events {
            let Some(target) = targets
                .iter()
                .find(|target| target.surface == event.surface)
            else {
                continue;
            };
            match &event.kind {
                PointerEventKind::Enter { .. } => {
                    let Some(position) = valid_position(app, target.window, event.position) else {
                        self.reject("invalid-enter-position");
                        continue;
                    };
                    if self
                        .focus
                        .as_ref()
                        .is_some_and(|focus| focus.window != target.window)
                    {
                        self.cleanup(app, output, None);
                    }
                    self.enter(
                        app,
                        output,
                        (
                            target.surface.id().protocol_id(),
                            target.window,
                            target.edge,
                        ),
                        (position, output_position(target, position)),
                    );
                    handled = true;
                }
                PointerEventKind::Leave { .. } => {
                    if self
                        .focus
                        .as_ref()
                        .is_some_and(|focus| focus.surface_id == event.surface.id().protocol_id())
                    {
                        self.leave(app, output);
                        handled = true;
                    }
                }
                PointerEventKind::Motion { .. }
                | PointerEventKind::Press { .. }
                | PointerEventKind::Release { .. } => {
                    if self
                        .focus
                        .as_ref()
                        .is_none_or(|focus| focus.surface_id != event.surface.id().protocol_id())
                    {
                        continue;
                    }
                    let Some(position) = valid_position(app, target.window, event.position) else {
                        self.reject("invalid-event-position");
                        continue;
                    };
                    self.motion(app, target.window, position);
                    self.record_output_position(target, position);
                    match event.kind {
                        PointerEventKind::Press { button, .. } => {
                            if let Some(button) = translate_button(button) {
                                if !self.pressed.contains(&button) {
                                    self.pressed.push(button);
                                }
                                emit_window(
                                    app,
                                    MouseButtonInput {
                                        button,
                                        state: ButtonState::Pressed,
                                        window: target.window,
                                    },
                                );
                            } else {
                                self.reject("unrepresentable-button");
                            }
                        }
                        PointerEventKind::Release { button, .. } => {
                            if let Some(button) = translate_button(button) {
                                self.pressed.retain(|held| *held != button);
                                emit_window(
                                    app,
                                    MouseButtonInput {
                                        button,
                                        state: ButtonState::Released,
                                        window: target.window,
                                    },
                                );
                            } else {
                                self.reject("unrepresentable-button");
                            }
                        }
                        PointerEventKind::Motion { .. } => {}
                        _ => unreachable!(),
                    }
                    handled = true;
                }
                PointerEventKind::Axis {
                    horizontal,
                    vertical,
                    ..
                } => {
                    if self
                        .focus
                        .as_ref()
                        .is_none_or(|focus| focus.surface_id != event.surface.id().protocol_id())
                    {
                        continue;
                    }
                    let Some(position) = valid_position(app, target.window, event.position) else {
                        self.reject("invalid-axis-position");
                        continue;
                    };
                    self.record_output_position(target, position);
                    self.axis(app, target.window, *horizontal, *vertical);
                    handled = true;
                }
            }
        }
        handled
    }

    fn enter(
        &mut self,
        app: &mut App,
        output: &OutputKey,
        target: (u32, Entity, Edge),
        positions: (Vec2, Vec2),
    ) {
        let (surface_id, window, edge) = target;
        let (position, output_position) = positions;
        set_cursor(app, window, Some(position));
        emit_window(app, CursorEntered { window });
        emit_window(
            app,
            CursorMoved {
                window,
                position,
                delta: None,
            },
        );
        semantic(app, output, edge, PanelInput::PointerEntered);
        self.last_output_position = Some(output_position);
        self.focus = Some(Focus {
            surface_id,
            window,
            edge,
            position,
            output_position,
        });
    }

    pub fn cleanup(&mut self, app: &mut App, output: &OutputKey, window: Option<Entity>) -> bool {
        if window.is_some_and(|window| {
            self.focus
                .as_ref()
                .is_none_or(|focus| focus.window != window)
        }) {
            return false;
        }
        let Some(focus) = self.focus.clone() else {
            return false;
        };
        self.motion(app, focus.window, focus.position);
        if !self.pressed.is_empty() {
            cancel_picking_press(app);
        }
        self.release_state(app, focus.window);
        self.leave(app, output);
        true
    }

    fn release_state(&mut self, app: &mut App, window: Entity) {
        for button in self.pressed.drain(..) {
            emit_window(
                app,
                MouseButtonInput {
                    button,
                    state: ButtonState::Released,
                    window,
                },
            );
        }
        if self.axis_active {
            emit_window(
                app,
                MouseWheel {
                    unit: MouseScrollUnit::Pixel,
                    x: 0.0,
                    y: 0.0,
                    window,
                    phase: TouchPhase::Ended,
                },
            );
            self.axis_active = false;
        }
    }

    fn motion(&mut self, app: &mut App, window: Entity, position: Vec2) {
        let delta = self
            .focus
            .as_ref()
            .filter(|focus| focus.window == window)
            .map(|focus| position - focus.position);
        set_cursor(app, window, Some(position));
        emit_window(
            app,
            CursorMoved {
                window,
                position,
                delta,
            },
        );
        if let Some(focus) = self.focus.as_mut() {
            focus.position = position;
        }
    }

    fn record_output_position(&mut self, target: &SurfaceTarget, local: Vec2) {
        let position = output_position(target, local);
        self.last_output_position = Some(position);
        if let Some(focus) = self.focus.as_mut() {
            focus.output_position = position;
        }
    }

    pub(crate) const fn last_output_position(&self) -> Option<Vec2> {
        self.last_output_position
    }

    fn leave(&mut self, app: &mut App, output: &OutputKey) {
        let Some(focus) = self.focus.take() else {
            return;
        };
        self.release_state(app, focus.window);
        set_cursor(app, focus.window, None);
        emit_window(
            app,
            CursorLeft {
                window: focus.window,
            },
        );
        semantic(app, output, focus.edge, PanelInput::PointerLeft);
    }

    fn axis(
        &mut self,
        app: &mut App,
        window: Entity,
        horizontal: AxisScroll,
        vertical: AxisScroll,
    ) {
        let stopped = horizontal.stop || vertical.stop;
        let phase = if stopped {
            TouchPhase::Ended
        } else if self.axis_active {
            TouchPhase::Moved
        } else {
            TouchPhase::Started
        };
        let discrete = horizontal.discrete != 0 || vertical.discrete != 0;
        emit_window(
            app,
            MouseWheel {
                unit: if discrete {
                    MouseScrollUnit::Line
                } else {
                    MouseScrollUnit::Pixel
                },
                x: -(if discrete {
                    horizontal.discrete as f32
                } else {
                    horizontal.absolute as f32
                }),
                y: -(if discrete {
                    vertical.discrete as f32
                } else {
                    vertical.absolute as f32
                }),
                window,
                phase,
            },
        );
        self.axis_active = !stopped;
    }

    fn reject(&mut self, reason: &'static str) {
        self.diagnostics = self.diagnostics.saturating_add(1);
        if self.diagnostics.is_power_of_two() {
            tracing::warn!(
                event = "quoin_pointer_event_rejected",
                count = self.diagnostics,
                reason
            );
        }
    }
}

fn cancel_picking_press(app: &mut App) {
    let location = {
        let world = app.world_mut();
        let mut pointers = world.query::<(&PointerId, &PointerLocation)>();
        pointers
            .iter(world)
            .find_map(|(id, location)| (*id == PointerId::Mouse).then(|| location.location.clone()))
            .flatten()
    };
    if let Some(location) = location
        && let Some(mut messages) = app
            .world_mut()
            .get_resource_mut::<bevy::ecs::message::Messages<PointerInput>>()
    {
        messages.write(PointerInput::new(
            PointerId::Mouse,
            location,
            PointerAction::Cancel,
        ));
    }
    if let Some(mut state) = app.world_mut().get_resource_mut::<PointerState>() {
        state.clear(PointerId::Mouse);
    }
    let world = app.world_mut();
    let mut pointers = world.query::<(&PointerId, &mut PointerPress)>();
    for (id, mut press) in pointers.iter_mut(world) {
        if *id == PointerId::Mouse {
            *press = PointerPress::default();
        }
    }
}

fn valid_position(app: &App, window: Entity, position: (f64, f64)) -> Option<Vec2> {
    if !position.0.is_finite() || !position.1.is_finite() {
        return None;
    }
    let position = Vec2::new(position.0 as f32, position.1 as f32);
    if !position.is_finite() {
        return None;
    }
    let window = app.world().get::<Window>(window)?;
    Some(Vec2::new(
        position.x.clamp(0.0, window.width()),
        position.y.clamp(0.0, window.height()),
    ))
}

fn set_cursor(app: &mut App, window: Entity, position: Option<Vec2>) {
    if let Some(mut window) = app.world_mut().get_mut::<Window>(window) {
        window.set_cursor_position(position);
    }
}

fn emit_window<T>(app: &mut App, event: T)
where
    T: bevy::prelude::Message + Clone,
    WindowEvent: From<T>,
{
    app.world_mut().write_message(event.clone());
    app.world_mut().write_message(WindowEvent::from(event));
}

fn semantic(app: &mut App, output: &OutputKey, edge: Edge, input: PanelInput) {
    stage_shell_command(app, output.clone(), ShellCommandKind::Panel { edge, input });
}

fn output_position(target: &SurfaceTarget, local: Vec2) -> Vec2 {
    output_logical_position(
        target.edge,
        local,
        target.output_size,
        target.thickness,
        target.committed_margin,
    )
}

fn translate_button(button: u32) -> Option<MouseButton> {
    Some(match button {
        BTN_LEFT => MouseButton::Left,
        BTN_RIGHT => MouseButton::Right,
        BTN_MIDDLE => MouseButton::Middle,
        BTN_SIDE | BTN_BACK => MouseButton::Back,
        BTN_EXTRA | BTN_FORWARD => MouseButton::Forward,
        other => MouseButton::Other(u16::try_from(other).ok()?),
    })
}

/// Convert a surface-local logical point to output-logical coordinates.
pub fn output_logical_position(
    edge: Edge,
    local: Vec2,
    output: Vec2,
    thickness: f32,
    committed_margin: i32,
) -> Vec2 {
    let margin = committed_margin as f32;
    match edge {
        Edge::Left => Vec2::new(margin + local.x, local.y),
        Edge::Right => Vec2::new(output.x - thickness - margin + local.x, local.y),
        Edge::Top => Vec2::new(local.x, margin + local.y),
        Edge::Bottom => Vec2::new(local.x, output.y - thickness - margin + local.y),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::MinimalPlugins;
    use bevy::camera::RenderTarget;
    use bevy::ecs::message::Messages;
    use bevy::ecs::observer::On;
    use bevy::picking::Pickable;
    use bevy::picking::PickingSettings;
    use bevy::picking::backend::HitData;
    use bevy::picking::events::{
        Cancel, Click, Drag, DragDrop, DragEnd, DragEnter, DragLeave, DragOver, DragStart, Enter,
        Leave, Move, Out, Over, Pointer, Press, Release, Scroll, pointer_events,
    };
    use bevy::picking::hover::{HoverMap, PreviousHoverMap};
    use bevy::picking::input::mouse_pick_events;
    use bevy::picking::pointer::{Location, PointerMap, update_pointer_map};
    use bevy::time::TimeUpdateStrategy;
    use bevy::ui_widgets::{Activate, Button, ButtonPlugin};
    use bevy::window::WindowRef;
    use cosmix_shell::core::{
        Corner, CornerEvent, CornerTrigger, LogicalSize, PanelMode, ShellModel,
    };
    use cosmix_shell::runtime::{ShellFrameState, ShellRuntimePlugin, WakePolicy};
    use std::time::Duration;

    fn pointer_app() -> (App, Entity, OutputKey) {
        let mut app = App::new();
        app.init_resource::<StagedShellCommands>();
        app.add_message::<CursorEntered>()
            .add_message::<CursorMoved>()
            .add_message::<CursorLeft>()
            .add_message::<MouseButtonInput>()
            .add_message::<MouseWheel>()
            .add_message::<WindowEvent>()
            .add_message::<ShellCommand>();
        let window = app
            .world_mut()
            .spawn(Window {
                resolution: bevy::window::WindowResolution::new(200, 100)
                    .with_scale_factor_override(2.0),
                ..Default::default()
            })
            .id();
        (app, window, OutputKey::new("DP-1").unwrap())
    }

    #[derive(Resource, Default)]
    struct ActivationCount(u32);

    fn count_activation(_activation: On<Activate>, mut count: ResMut<ActivationCount>) {
        count.0 += 1;
    }

    fn picking_app() -> (App, Entity, Entity, OutputKey) {
        let (mut app, window, output) = pointer_app();
        app.add_plugins(ButtonPlugin)
            .init_resource::<PickingSettings>()
            .init_resource::<PointerState>()
            .init_resource::<PointerMap>()
            .init_resource::<HoverMap>()
            .init_resource::<PreviousHoverMap>()
            .init_resource::<ActivationCount>()
            .add_message::<PointerInput>()
            .add_message::<Pointer<Cancel>>()
            .add_message::<Pointer<Click>>()
            .add_message::<Pointer<Press>>()
            .add_message::<Pointer<DragDrop>>()
            .add_message::<Pointer<DragEnd>>()
            .add_message::<Pointer<DragEnter>>()
            .add_message::<Pointer<Drag>>()
            .add_message::<Pointer<DragLeave>>()
            .add_message::<Pointer<DragOver>>()
            .add_message::<Pointer<DragStart>>()
            .add_message::<Pointer<Scroll>>()
            .add_message::<Pointer<Move>>()
            .add_message::<Pointer<Out>>()
            .add_message::<Pointer<Over>>()
            .add_message::<Pointer<Leave>>()
            .add_message::<Pointer<Enter>>()
            .add_message::<Pointer<Release>>()
            .add_observer(count_activation);
        app.finish();
        app.cleanup();

        let target = RenderTarget::Window(WindowRef::Entity(window))
            .normalize(None)
            .unwrap();
        let location = Location {
            target,
            position: Vec2::new(12.0, 8.0),
        };
        app.world_mut().spawn((
            PointerId::Mouse,
            PointerLocation::new(location),
            PointerPress::default(),
        ));
        app.world_mut()
            .run_system_cached(update_pointer_map)
            .unwrap();
        let button = app.world_mut().spawn((Button, Pickable::default())).id();
        let hit = HitData::new(button, 0.0, None, None);
        app.world_mut()
            .resource_mut::<HoverMap>()
            .entry(PointerId::Mouse)
            .or_default()
            .insert(button, hit.clone());
        app.world_mut()
            .resource_mut::<PreviousHoverMap>()
            .entry(PointerId::Mouse)
            .or_default()
            .insert(button, hit);
        (app, window, button, output)
    }

    #[test]
    fn output_logical_conversion_covers_every_edge_and_margin_sign() {
        let local = Vec2::new(5.0, 7.0);
        let output = Vec2::new(1_000.0, 800.0);
        assert_eq!(
            output_logical_position(Edge::Left, local, output, 100.0, -20),
            Vec2::new(-15.0, 7.0)
        );
        assert_eq!(
            output_logical_position(Edge::Top, local, output, 100.0, 0),
            Vec2::new(5.0, 7.0)
        );
        assert_eq!(
            output_logical_position(Edge::Right, local, output, 100.0, -20),
            Vec2::new(925.0, 7.0)
        );
        assert_eq!(
            output_logical_position(Edge::Bottom, local, output, 100.0, 0),
            Vec2::new(5.0, 707.0)
        );
    }

    #[test]
    fn staged_corner_times_start_after_idle_and_left_gets_a_fresh_grace() {
        let output = OutputKey::new("DP-1").unwrap();
        let model = ShellModel::new(
            output.clone(),
            LogicalSize::new(1_000.0, 800.0).unwrap(),
            Duration::ZERO,
            Duration::from_millis(800),
            Duration::from_millis(200),
        )
        .unwrap();
        let mut app = App::new();
        configure_ingress(&mut app);
        app.add_plugins((MinimalPlugins, ShellRuntimePlugin::new(model)))
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs(1)));

        app.update();
        stage_shell_command(
            &mut app,
            output.clone(),
            ShellCommandKind::Corner(CornerEvent::Entered {
                corner: Corner::TopLeft,
                dwell: Duration::from_millis(200),
                trigger: CornerTrigger::Compositor,
            }),
        );
        app.update();
        let entered = app
            .world()
            .resource::<ShellFrameState>()
            .0
            .panel(Edge::Left);
        assert_eq!(entered.mode, PanelMode::Revealed);
        assert_eq!(
            entered.visible_fraction, 0.0,
            "late enter starts its animation now"
        );

        app.update();
        assert_eq!(
            app.world()
                .resource::<ShellFrameState>()
                .0
                .panel(Edge::Left)
                .mode,
            PanelMode::Revealed,
            "an active corner hold survives more than one grace interval"
        );
        *app.world_mut().resource_mut::<TimeUpdateStrategy>() =
            TimeUpdateStrategy::ManualDuration(Duration::ZERO);
        stage_shell_command(
            &mut app,
            output,
            ShellCommandKind::Corner(CornerEvent::Left {
                corner: Corner::TopLeft,
            }),
        );
        app.update();
        let now = app.world().resource::<Time<Real>>().elapsed();
        let frame = &app.world().resource::<ShellFrameState>().0;
        let left = frame.panel(Edge::Left);
        assert_eq!(left.mode, PanelMode::Revealed);
        assert_eq!(
            frame.wake,
            WakePolicy::WakeAt(now + Duration::from_millis(800))
        );
    }

    #[test]
    fn enter_order_cursor_scale_delta_and_window_attribution_are_exact() {
        let (mut app, window, output) = pointer_app();
        let mut bridge = PointerBridge::default();
        bridge.enter(
            &mut app,
            &output,
            (7, window, Edge::Left),
            (Vec2::new(20.0, 10.0), Vec2::new(20.0, 10.0)),
        );
        bridge.motion(&mut app, window, Vec2::new(24.0, 13.0));
        let events = app
            .world_mut()
            .resource_mut::<Messages<WindowEvent>>()
            .drain()
            .collect::<Vec<_>>();
        assert!(
            matches!(events[0], WindowEvent::CursorEntered(CursorEntered { window: event_window }) if event_window == window)
        );
        assert!(
            matches!(events[1], WindowEvent::CursorMoved(CursorMoved { window: event_window, position, delta: None }) if event_window == window && position == Vec2::new(20.0, 10.0))
        );
        assert!(
            matches!(events[2], WindowEvent::CursorMoved(CursorMoved { window: event_window, position, delta: Some(delta) }) if event_window == window && position == Vec2::new(24.0, 13.0) && delta == Vec2::new(4.0, 3.0))
        );
        assert_eq!(
            app.world()
                .get::<Window>(window)
                .unwrap()
                .physical_cursor_position(),
            Some(Vec2::new(48.0, 26.0))
        );
    }

    #[test]
    fn wheel_sign_unit_phase_and_cleanup_releases_then_leaves() {
        let (mut app, window, output) = pointer_app();
        let mut bridge = PointerBridge::default();
        bridge.enter(
            &mut app,
            &output,
            (7, window, Edge::Top),
            (Vec2::new(20.0, 10.0), Vec2::new(20.0, 10.0)),
        );
        bridge.axis(
            &mut app,
            window,
            AxisScroll {
                discrete: 2,
                ..Default::default()
            },
            AxisScroll {
                discrete: -3,
                ..Default::default()
            },
        );
        bridge
            .pressed
            .extend([MouseButton::Left, MouseButton::Back]);
        assert!(bridge.cleanup(&mut app, &output, Some(window)));
        let events = app
            .world_mut()
            .resource_mut::<Messages<WindowEvent>>()
            .drain()
            .collect::<Vec<_>>();
        assert!(events.iter().any(|event| matches!(event, WindowEvent::MouseWheel(MouseWheel { unit: MouseScrollUnit::Line, x, y, phase: TouchPhase::Started, .. }) if *x == -2.0 && *y == 3.0)));
        let releases = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    WindowEvent::MouseButtonInput(MouseButtonInput {
                        state: ButtonState::Released,
                        ..
                    })
                )
            })
            .unwrap();
        let left = events
            .iter()
            .position(|event| matches!(event, WindowEvent::CursorLeft(_)))
            .unwrap();
        assert!(releases < left);
        assert!(bridge.focus.is_none());
        assert!(bridge.pressed.is_empty());
        assert_eq!(
            app.world()
                .get::<Window>(window)
                .unwrap()
                .physical_cursor_position(),
            None
        );
    }

    #[test]
    fn native_leave_releases_buttons_and_axis_before_later_teardown() {
        let (mut app, window, output) = pointer_app();
        let mut bridge = PointerBridge::default();
        bridge.enter(
            &mut app,
            &output,
            (7, window, Edge::Right),
            (Vec2::new(10.0, 20.0), Vec2::new(10.0, 20.0)),
        );
        bridge.pressed.push(MouseButton::Left);
        bridge.axis_active = true;
        bridge.leave(&mut app, &output);

        let events = app
            .world_mut()
            .resource_mut::<Messages<WindowEvent>>()
            .drain()
            .collect::<Vec<_>>();
        let release = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    WindowEvent::MouseButtonInput(MouseButtonInput {
                        state: ButtonState::Released,
                        ..
                    })
                )
            })
            .unwrap();
        let axis_end = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    WindowEvent::MouseWheel(MouseWheel {
                        phase: TouchPhase::Ended,
                        ..
                    })
                )
            })
            .unwrap();
        let left = events
            .iter()
            .position(|event| matches!(event, WindowEvent::CursorLeft(_)))
            .unwrap();
        assert!(release < left);
        assert!(axis_end < left);
        assert!(bridge.pressed.is_empty());
        assert!(!bridge.axis_active);
        assert!(!bridge.cleanup(&mut app, &output, None));
    }

    #[test]
    fn teardown_cancels_a_pressed_control_without_activating_it() {
        let (mut app, window, button, output) = picking_app();
        let mut bridge = PointerBridge::default();
        bridge.enter(
            &mut app,
            &output,
            (7, window, Edge::Left),
            (Vec2::new(12.0, 8.0), Vec2::new(12.0, 8.0)),
        );
        app.world_mut()
            .resource_mut::<Messages<WindowEvent>>()
            .clear();

        emit_window(
            &mut app,
            MouseButtonInput {
                button: MouseButton::Left,
                state: ButtonState::Pressed,
                window,
            },
        );
        app.world_mut()
            .run_system_cached(mouse_pick_events)
            .unwrap();
        app.world_mut()
            .run_system_cached(PointerInput::receive)
            .unwrap();
        app.world_mut().run_system_cached(pointer_events).unwrap();
        app.world_mut().flush();
        assert!(app.world().entity(button).contains::<bevy::ui::Pressed>());

        bridge.pressed.push(MouseButton::Left);
        assert!(bridge.cleanup(&mut app, &output, Some(window)));
        app.world_mut()
            .run_system_cached(mouse_pick_events)
            .unwrap();
        app.world_mut()
            .run_system_cached(PointerInput::receive)
            .unwrap();
        app.world_mut().run_system_cached(pointer_events).unwrap();
        app.world_mut().flush();

        assert_eq!(app.world().resource::<ActivationCount>().0, 0);
        assert!(!app.world().entity(button).contains::<bevy::ui::Pressed>());
        let mut pointers = app.world_mut().query::<(&PointerId, &PointerPress)>();
        let press = pointers
            .iter(app.world())
            .find_map(|(id, press)| (*id == PointerId::Mouse).then_some(press))
            .unwrap();
        assert!(!press.is_any_pressed());
    }

    fn keyboard_app() -> (App, Entity) {
        let mut app = App::new();
        app.add_message::<KeyboardInput>()
            .add_message::<KeyboardFocusLost>()
            .add_message::<WindowFocused>()
            .add_message::<WindowEvent>()
            .add_message::<TouchInput>();
        app.add_systems(PreUpdate, coalesce_keyboard_focus_lost.before(InputSystems));
        let window = app
            .world_mut()
            .spawn(Window {
                resolution: bevy::window::WindowResolution::new(200, 100),
                ..Default::default()
            })
            .id();
        (app, window)
    }

    fn us_keymap() -> xkb::Keymap {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        xkb::Keymap::new_from_names(&context, "", "", "us", "", None, xkb::COMPILE_NO_FLAGS)
            .expect("the test host supplies the standard US xkb keymap")
    }

    #[test]
    fn xkb_keymap_install_owns_a_compiled_copy_and_bounds_repeat_gap() {
        let keymap = us_keymap();
        let source = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
        let mut bridge = KeyboardBridge::default();
        assert!(bridge.install_keymap_text(source.clone()).installed);
        assert!(bridge.keymap.is_some());

        assert!(!bridge.update_repeat_info(
            RepeatInfo::Repeat {
                rate: std::num::NonZeroU32::new(u32::MAX).unwrap(),
                delay: 0,
            },
            Duration::ZERO,
        ));
        assert_eq!(
            bridge.repeat_settings,
            Some(RepeatSettings {
                delay: MIN_REPEAT_DELAY,
                gap: MIN_REPEAT_GAP,
            })
        );

        bridge.update_repeat_info(
            RepeatInfo::Repeat {
                rate: std::num::NonZeroU32::new(1).unwrap(),
                delay: u32::MAX,
            },
            Duration::ZERO,
        );
        assert_eq!(
            bridge.repeat_settings,
            Some(RepeatSettings {
                delay: MAX_REPEAT_DELAY,
                gap: MAX_REPEAT_GAP,
            })
        );
        bridge.repeating = Some(RepeatingKey {
            key: map_key(30, Keysym::a, Some("a".to_owned())),
            next_deadline: Duration::from_secs(3),
        });
        assert_eq!(
            bridge.install_keymap_text(source),
            KeymapUpdate {
                installed: true,
                deadline_changed: false,
            }
        );
        assert_eq!(bridge.repeat_deadline(), Some(Duration::from_secs(3)));
    }

    #[test]
    fn pathological_repeat_info_is_clamped_and_cannot_refire_at_one_instant() {
        let (mut app, window) = keyboard_app();
        let mut bridge = KeyboardBridge {
            focus: Some(KeyboardFocus {
                surface: None,
                window,
            }),
            keymap: Some(us_keymap()),
            ..Default::default()
        };
        bridge.update_repeat_info(
            RepeatInfo::Repeat {
                rate: std::num::NonZeroU32::new(u32::MAX).unwrap(),
                delay: 0,
            },
            Duration::ZERO,
        );
        bridge.press(
            &mut app,
            SctkKeyEvent {
                time: 1,
                raw_code: 30,
                keysym: Keysym::a,
                utf8: Some("a".to_owned()),
            },
            Duration::ZERO,
        );

        assert_eq!(bridge.repeat_deadline(), Some(MIN_REPEAT_DELAY));
        assert!(bridge.fire_repeat(&mut app, MIN_REPEAT_DELAY));
        assert_eq!(
            bridge.repeat_deadline(),
            Some(MIN_REPEAT_DELAY + MIN_REPEAT_GAP)
        );
        assert!(!bridge.fire_repeat(&mut app, MIN_REPEAT_DELAY));
    }

    #[test]
    fn xkb_key_text_and_repeat_mapping_is_exact() {
        let (mut app, window) = keyboard_app();
        let keymap = us_keymap();
        let mut bridge = KeyboardBridge {
            focus: Some(KeyboardFocus {
                surface: None,
                window,
            }),
            keymap: Some(keymap),
            ..Default::default()
        };
        bridge.update_repeat_info(
            RepeatInfo::Repeat {
                rate: std::num::NonZeroU32::new(20).unwrap(),
                delay: 200,
            },
            Duration::from_secs(1),
        );
        let press = SctkKeyEvent {
            time: 10,
            raw_code: 30,
            keysym: Keysym::A,
            utf8: Some("A".to_owned()),
        };

        assert!(bridge.press(&mut app, press.clone(), Duration::from_secs(1)));
        assert_eq!(bridge.repeat_deadline(), Some(Duration::from_millis(1_200)));
        assert!(!bridge.fire_repeat(&mut app, Duration::from_millis(1_199)));
        assert!(bridge.fire_repeat(&mut app, Duration::from_millis(1_200)));
        bridge.update_modifiers(Modifiers::default(), 0);
        assert_eq!(bridge.repeat_deadline(), None);

        let events = app
            .world_mut()
            .resource_mut::<Messages<KeyboardInput>>()
            .drain()
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].window, window);
        assert_eq!(events[0].key_code, KeyCode::KeyA);
        assert_eq!(events[0].logical_key, Key::Character("A".into()));
        assert_eq!(events[0].text.as_deref(), Some("A"));
        assert!(!events[0].repeat);
        assert!(events[1].repeat);
        assert_eq!(events[1].logical_key, Key::Character("A".into()));
        assert_eq!(events[1].text.as_deref(), Some("A"));

        assert!(bridge.release(
            &mut app,
            SctkKeyEvent {
                utf8: None,
                ..press
            }
        ));
        assert_eq!(bridge.repeat_deadline(), None);
        assert!(bridge.pressed.is_empty());
    }

    #[test]
    fn non_repeating_press_preserves_candidate_until_any_modifier_callback() {
        let (mut app, window) = keyboard_app();
        let mut bridge = KeyboardBridge {
            focus: Some(KeyboardFocus {
                surface: None,
                window,
            }),
            keymap: Some(us_keymap()),
            repeat_settings: Some(RepeatSettings {
                delay: Duration::from_millis(200),
                gap: Duration::from_millis(50),
            }),
            ..Default::default()
        };
        bridge.press(
            &mut app,
            SctkKeyEvent {
                time: 10,
                raw_code: 30,
                keysym: Keysym::a,
                utf8: Some("a".to_owned()),
            },
            Duration::ZERO,
        );
        assert!(bridge.repeat_deadline().is_some());

        bridge.press(
            &mut app,
            SctkKeyEvent {
                time: 11,
                raw_code: 42,
                keysym: Keysym::Shift_L,
                utf8: None,
            },
            Duration::from_millis(10),
        );
        assert_eq!(bridge.repeat_deadline(), Some(Duration::from_millis(200)));
        assert!(bridge.update_modifiers(
            Modifiers {
                shift: true,
                ..Default::default()
            },
            0,
        ));

        assert_eq!(bridge.repeat_deadline(), None);
        bridge.press(
            &mut app,
            SctkKeyEvent {
                time: 12,
                raw_code: 30,
                keysym: Keysym::a,
                utf8: Some("a".to_owned()),
            },
            Duration::from_millis(20),
        );
        assert!(bridge.repeat_deadline().is_some());
        assert!(bridge.update_modifiers(
            Modifiers {
                shift: true,
                ..Default::default()
            },
            0,
        ));
        assert_eq!(bridge.repeat_deadline(), None);
        let events = app
            .world_mut()
            .resource_mut::<Messages<KeyboardInput>>()
            .drain()
            .collect::<Vec<_>>();
        assert_eq!(events[1].logical_key, Key::Shift);
        assert_eq!(events[1].key_code, KeyCode::ShiftLeft);
    }

    #[test]
    fn named_and_dead_keys_do_not_collapse_to_unidentified() {
        assert_eq!(
            logical_key(Keysym::XF86_AudioRaiseVolume),
            Key::AudioVolumeUp
        );
        assert!(matches!(logical_key(Keysym::dead_lowline), Key::Dead(_)));
        assert_eq!(logical_key(Keysym::F35), Key::F35);
        assert_eq!(logical_key(Keysym::space), Key::Space);
        assert_eq!(logical_key(Keysym::XF86_AudioPlay), Key::MediaPlay);
    }

    #[test]
    fn keyboard_focus_loss_stops_repeat_and_clears_held_state() {
        let (mut app, window) = keyboard_app();
        let mut bridge = KeyboardBridge {
            focus: Some(KeyboardFocus {
                surface: None,
                window,
            }),
            keymap: Some(us_keymap()),
            repeat_settings: Some(RepeatSettings {
                delay: Duration::from_millis(200),
                gap: Duration::from_millis(50),
            }),
            ..Default::default()
        };
        bridge.press(
            &mut app,
            SctkKeyEvent {
                time: 10,
                raw_code: 30,
                keysym: Keysym::a,
                utf8: Some("a".to_owned()),
            },
            Duration::ZERO,
        );

        assert!(bridge.cleanup(&mut app, Some(window)));
        assert!(bridge.focus.is_none());
        assert!(bridge.pressed.is_empty());
        app.update();
        let events = app
            .world_mut()
            .resource_mut::<Messages<KeyboardInput>>()
            .drain()
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].state, ButtonState::Pressed);
        assert_eq!(events[1].state, ButtonState::Released);
        assert_eq!(events[0].key_code, events[1].key_code);
        assert_eq!(bridge.repeat_deadline(), None);
        assert!(!app.world().get::<Window>(window).unwrap().focused);
        assert_eq!(
            app.world_mut()
                .resource_mut::<Messages<KeyboardFocusLost>>()
                .drain()
                .count(),
            1
        );
        let focused = app
            .world_mut()
            .resource_mut::<Messages<WindowFocused>>()
            .drain()
            .collect::<Vec<_>>();
        assert_eq!(
            focused,
            [WindowFocused {
                window,
                focused: false
            }]
        );
    }

    #[test]
    fn focus_loss_and_touch_cancel_clear_bevy_pressed_resources() {
        let mut app = App::new();
        app.add_plugins(bevy::input::InputPlugin)
            .add_message::<WindowFocused>()
            .add_message::<WindowEvent>();
        app.add_systems(PreUpdate, coalesce_keyboard_focus_lost.before(InputSystems));
        let window = app
            .world_mut()
            .spawn(Window {
                resolution: bevy::window::WindowResolution::new(200, 100),
                ..Default::default()
            })
            .id();
        let keymap = us_keymap();
        let mut keyboard = KeyboardBridge {
            focus: Some(KeyboardFocus {
                surface: None,
                window,
            }),
            keymap: Some(keymap),
            ..Default::default()
        };
        keyboard.press(
            &mut app,
            SctkKeyEvent {
                time: 10,
                raw_code: 30,
                keysym: Keysym::a,
                utf8: Some("a".to_owned()),
            },
            Duration::ZERO,
        );
        let mut touch = TouchBridge::default();
        touch.start_contact(&mut app, window, 8, Vec2::new(5.0, 7.0));
        app.update();
        assert!(
            app.world()
                .resource::<bevy::input::ButtonInput<KeyCode>>()
                .pressed(KeyCode::KeyA)
        );
        assert!(
            app.world()
                .resource::<bevy::input::touch::Touches>()
                .get_pressed(8)
                .is_some()
        );

        assert!(keyboard.cleanup(&mut app, Some(window)));
        assert!(touch.cancel(&mut app));
        app.update();
        assert!(
            app.world()
                .resource::<bevy::input::ButtonInput<KeyCode>>()
                .get_pressed()
                .next()
                .is_none()
        );
        assert!(
            app.world()
                .resource::<bevy::input::touch::Touches>()
                .get_pressed(8)
                .is_none()
        );
    }

    #[test]
    fn leave_then_enter_before_update_preserves_both_pressed_key_resources() {
        let mut app = App::new();
        app.add_plugins(bevy::input::InputPlugin)
            .add_message::<WindowFocused>()
            .add_message::<WindowEvent>();
        app.add_systems(PreUpdate, coalesce_keyboard_focus_lost.before(InputSystems));
        let old_window = app.world_mut().spawn(Window::default()).id();
        let new_window = app.world_mut().spawn(Window::default()).id();
        let old_key = MappedKey {
            raw_code: 30,
            key_code: KeyCode::KeyA,
            logical_key: Key::Character("a".into()),
            text: Some("a".to_owned()),
        };
        let new_key = MappedKey {
            raw_code: 45,
            key_code: KeyCode::KeyX,
            logical_key: Key::Character("x".into()),
            text: Some("x".to_owned()),
        };

        emit_keyboard(&mut app, old_window, &old_key, ButtonState::Pressed, false);
        app.update();

        emit_keyboard(&mut app, old_window, &old_key, ButtonState::Released, false);
        emit_window(
            &mut app,
            WindowFocused {
                window: old_window,
                focused: false,
            },
        );
        emit_window(
            &mut app,
            WindowFocused {
                window: new_window,
                focused: true,
            },
        );
        emit_keyboard(&mut app, new_window, &new_key, ButtonState::Pressed, false);
        app.update();

        let key_codes = app.world().resource::<bevy::input::ButtonInput<KeyCode>>();
        assert!(!key_codes.pressed(KeyCode::KeyA));
        assert!(key_codes.pressed(KeyCode::KeyX));
        let logical_keys = app.world().resource::<bevy::input::ButtonInput<Key>>();
        assert!(!logical_keys.pressed(Key::Character("a".into())));
        assert!(logical_keys.pressed(Key::Character("x".into())));
        assert_eq!(
            app.world_mut()
                .resource_mut::<Messages<KeyboardFocusLost>>()
                .drain()
                .count(),
            0
        );
    }

    #[test]
    fn leave_update_then_later_enter_preserves_both_pressed_key_resources() {
        let mut app = App::new();
        app.add_plugins(bevy::input::InputPlugin)
            .add_message::<WindowFocused>()
            .add_message::<WindowEvent>();
        app.add_systems(PreUpdate, coalesce_keyboard_focus_lost.before(InputSystems));
        let old_window = app.world_mut().spawn(Window::default()).id();
        let new_window = app.world_mut().spawn(Window::default()).id();
        let old_key = MappedKey {
            raw_code: 30,
            key_code: KeyCode::KeyA,
            logical_key: Key::Character("a".into()),
            text: Some("a".to_owned()),
        };
        let new_key = MappedKey {
            raw_code: 45,
            key_code: KeyCode::KeyX,
            logical_key: Key::Character("x".into()),
            text: Some("x".to_owned()),
        };

        emit_keyboard(&mut app, old_window, &old_key, ButtonState::Pressed, false);
        app.update();

        // Dispatch 1: its requested update must write and consume the global
        // loss marker before the runner is allowed to return to dispatch.
        emit_keyboard(&mut app, old_window, &old_key, ButtonState::Released, false);
        emit_window(
            &mut app,
            WindowFocused {
                window: old_window,
                focused: false,
            },
        );
        app.update();

        // Dispatch 2: a later focus gain and press cannot meet a stale marker.
        emit_window(
            &mut app,
            WindowFocused {
                window: new_window,
                focused: true,
            },
        );
        emit_keyboard(&mut app, new_window, &new_key, ButtonState::Pressed, false);
        app.update();

        let key_codes = app.world().resource::<bevy::input::ButtonInput<KeyCode>>();
        assert!(!key_codes.pressed(KeyCode::KeyA));
        assert!(key_codes.pressed(KeyCode::KeyX));
        let logical_keys = app.world().resource::<bevy::input::ButtonInput<Key>>();
        assert!(!logical_keys.pressed(Key::Character("a".into())));
        assert!(logical_keys.pressed(Key::Character("x".into())));
    }

    #[test]
    fn touch_attribution_motion_and_cancel_clear_pressed_contacts() {
        let (mut app, left) = keyboard_app();
        let right = app
            .world_mut()
            .spawn(Window {
                resolution: bevy::window::WindowResolution::new(200, 100),
                ..Default::default()
            })
            .id();
        let mut bridge = TouchBridge::default();
        bridge.start_contact(&mut app, right, -4, Vec2::new(5.0, 7.0));
        assert!(bridge.motion(&mut app, -4, (9.0, 10.0)));
        assert_eq!(bridge.contacts[&-4].window, right);
        assert_ne!(bridge.contacts[&-4].window, left);
        assert_eq!(bridge.contacts[&-4].position, Vec2::new(9.0, 10.0));
        assert!(bridge.cancel(&mut app));
        assert!(bridge.contacts.is_empty());

        let events = app
            .world_mut()
            .resource_mut::<Messages<TouchInput>>()
            .drain()
            .collect::<Vec<_>>();
        assert_eq!(
            events.iter().map(|event| event.phase).collect::<Vec<_>>(),
            [TouchPhase::Started, TouchPhase::Moved, TouchPhase::Canceled]
        );
        assert!(events.iter().all(|event| event.window == right));
        assert!(events.iter().all(|event| event.id == (-4_i32) as u64));
    }

    #[test]
    fn invalid_positions_and_unknown_wide_buttons_are_rejected() {
        let (app, window, _) = pointer_app();
        assert_eq!(valid_position(&app, window, (f64::NAN, 1.0)), None);
        assert_eq!(
            valid_position(&app, window, (150.0, 75.0)),
            Some(Vec2::new(100.0, 50.0))
        );
        assert_eq!(translate_button(u32::MAX), None);
        assert_eq!(translate_button(BTN_LEFT), Some(MouseButton::Left));
    }
}
