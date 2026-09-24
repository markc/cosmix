//! `comp.input.*`: Bus-injected input, delivered through the real seat path.
//!
//! Ordinary injection enters [`WaylandState::handle_host_input`] like a
//! device event. Targeted buttons use the same seat's motion/button path
//! after focus arbitration, avoiding a second hit-test or unconditional
//! raise. Nothing here talks to a client directly.

use std::collections::VecDeque;

use serde_json::{Value, json};
use smithay::input::keyboard::{Keysym, xkb};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::wayland::input_method::InputMethodSeat as _;

use super::*;
use crate::port::{
    ControlReply, InputOp, KeySpec, LongOp, PointerMoveTarget, PressAction, ScrollSource,
    SequenceStep,
};
use crate::protocol::port_snapshot::{HostInputSnapshot, project_output, project_outputs};

/// `frame_trace` kind codes (the `detail` field of `comp_input_injected`).
#[derive(Clone, Copy, Debug)]
enum InjectedKind {
    PointerMove = 1,
    PointerButton = 2,
    PointerScroll = 3,
    Key = 4,
    Text = 5,
    ReleaseAll = 6,
}

/// The injected-input marks live in one place, the presentation stats
/// registry (`presentation_stats::StatsRegistry`): this module mints
/// `input_seq` and records the mark there, the stats consume it.
pub(crate) use super::presentation_stats::InputMark;

/// A sequence yields to the event loop (dispatch, client flush) after this
/// many injected events, so a long zero-delay run cannot fill a client's
/// socket inside one callback.
const SEQUENCE_YIELD_EVENTS: u64 = 256;
const SEQUENCE_YIELD: Duration = Duration::from_millis(1);

/// A key (raw XKB code) or button that injection pressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Hold {
    Key(u32),
    Button(u32),
}

/// Who pressed a hold: a sequence run, or `None` for single verbs.
type HoldOwner = Option<u64>;

/// Every injected hold and the owners that pressed it. A hold is released
/// on an owner's abort only when no other owner still holds it; an
/// explicit release (any caller) really lets the key go, so it clears
/// every owner.
#[derive(Default)]
pub(super) struct Holds {
    owners: BTreeMap<Hold, BTreeSet<HoldOwner>>,
}

impl Holds {
    fn note(&mut self, owner: HoldOwner, input: &HostInput) {
        let (hold, state) = match *input {
            HostInput::Key { keycode, state, .. } => (Hold::Key(keycode.raw()), state),
            HostInput::PointerButton { button, state, .. } => (Hold::Button(button), state),
            _ => return,
        };
        if state == HostButtonState::Pressed {
            self.owners.entry(hold).or_default().insert(owner);
        } else {
            self.owners.remove(&hold);
        }
    }

    /// Drop one owner; returns the holds nobody holds any more.
    fn drop_owner(&mut self, owner: HoldOwner) -> Vec<Hold> {
        let mut orphaned = Vec::new();
        self.owners.retain(|hold, owners| {
            if owners.remove(&owner) && owners.is_empty() {
                orphaned.push(*hold);
                return false;
            }
            !owners.is_empty()
        });
        orphaned
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.owners.is_empty()
    }

    #[cfg(test)]
    pub(super) fn owners_of(&self, hold: Hold) -> usize {
        self.owners.get(&hold).map_or(0, BTreeSet::len)
    }
}

pub(crate) struct InjectionState {
    next_seq: u64,
    /// Everything injection holds, by owner; `release_all` releases all.
    pub(super) held: Holds,
    /// The sequence whose step is running (the owner of what it presses).
    current_run: Option<u64>,
    /// Injected events so far (for the sequence yield).
    pub(super) events: u64,
    host_passthrough: bool,
    /// Host keys and buttons pressed while passthrough was on. Their
    /// releases always pass, so turning passthrough off never strands one.
    host_held_keys: BTreeSet<u32>,
    host_held_buttons: BTreeSet<u32>,
    /// Pointer moves skip hot-corner sampling while set (`corners: false`).
    pub(super) suppress_corners: bool,
    pub(super) sequences: HashMap<u64, SequenceRun>,
    next_sequence: u64,
}

impl Default for InjectionState {
    fn default() -> Self {
        Self {
            next_seq: 0,
            held: Holds::default(),
            current_run: None,
            events: 0,
            host_passthrough: true,
            host_held_keys: BTreeSet::new(),
            host_held_buttons: BTreeSet::new(),
            suppress_corners: false,
            sequences: HashMap::new(),
            next_sequence: 0,
        }
    }
}

pub(super) struct SequenceRun {
    steps: VecDeque<SequenceStep>,
    index: usize,
    /// The front step's delay has elapsed.
    delay_elapsed: bool,
    replies: Vec<Value>,
    started: Instant,
    reply: tokio::sync::oneshot::Sender<ControlReply>,
}

/// Keysym and character lookups against the live seat keymap, each
/// verified by an XKB state carrying the seat's locked modifiers and
/// layout, so Caps Lock and the active group are honoured.
struct KeymapIndex {
    by_sym: HashMap<u32, (Keycode, bool)>,
    by_char: HashMap<u32, (Keycode, bool)>,
}

fn build_keymap_index(keyboard: &smithay::input::keyboard::Xkb) -> KeymapIndex {
    // SAFETY: the references (and the scratch states' keymap ref-counts)
    // live only inside this call, under the keyboard's xkb lock.
    let (keymap, live) = unsafe { (keyboard.keymap(), keyboard.state()) };
    let locked = live.serialize_mods(xkb::STATE_MODS_LOCKED);
    let layout = live.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE);
    let shift_index = keymap.mod_get_index(xkb::MOD_NAME_SHIFT);
    let shift = if shift_index == xkb::MOD_INVALID {
        0
    } else {
        1 << shift_index
    };
    let mut index = KeymapIndex {
        by_sym: HashMap::new(),
        by_char: HashMap::new(),
    };
    let (min, max) = (keymap.min_keycode().raw(), keymap.max_keycode().raw());
    for uses_shift in [false, true] {
        if uses_shift && shift == 0 {
            continue;
        }
        let mut state = xkb::State::new(keymap);
        state.update_mask(if uses_shift { shift } else { 0 }, 0, locked, 0, 0, layout);
        for raw in min..=max {
            let keycode = Keycode::new(raw);
            let sym = state.key_get_one_sym(keycode);
            if sym == Keysym::NoSymbol {
                continue;
            }
            index
                .by_sym
                .entry(sym.raw())
                .or_insert((keycode, uses_shift));
            let character = xkb::keysym_to_utf32(sym);
            if character != 0 {
                index
                    .by_char
                    .entry(character)
                    .or_insert((keycode, uses_shift));
            }
        }
    }
    index
}

fn keysym_by_name(name: &str) -> Option<Keysym> {
    [xkb::KEYSYM_NO_FLAGS, xkb::KEYSYM_CASE_INSENSITIVE]
        .into_iter()
        .map(|flags| xkb::keysym_from_name(name, flags))
        .find(|sym| *sym != Keysym::NoSymbol)
}

fn press_states(action: PressAction) -> &'static [HostButtonState] {
    match action {
        PressAction::Press => &[HostButtonState::Pressed],
        PressAction::Release => &[HostButtonState::Released],
        PressAction::Both => &[HostButtonState::Pressed, HostButtonState::Released],
    }
}

impl WaylandState {
    fn keymap_index(&mut self) -> KeymapIndex {
        let keyboard = self.keyboard.clone();
        keyboard.with_xkb_state(self, |context| {
            let xkb = context
                .xkb()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            build_keymap_index(&xkb)
        })
    }

    /// Resolve a key to `(keycode, needs shift)` without sending anything.
    fn resolve_key(&self, index: &KeymapIndex, key: &KeySpec) -> Option<(Keycode, bool)> {
        match key {
            KeySpec::Evdev(code) => Some((Keycode::new(code + 8), false)),
            KeySpec::Name(name) => {
                let sym = keysym_by_name(name)?;
                index.by_sym.get(&sym.raw()).copied()
            }
        }
    }

    fn inject(&mut self, input: HostInput) {
        let owner = self.injection.current_run;
        self.injection.held.note(owner, &input);
        self.injection.events = self.injection.events.wrapping_add(1);
        self.handle_host_input(input);
    }

    fn inject_key(&mut self, keycode: Keycode, state: HostButtonState, time: u32) {
        self.inject(HostInput::Key {
            keycode,
            state,
            time,
        });
    }

    /// `comp.input.*` (one verb). Target candidacy is checked before focus;
    /// a refused target injects no key or button.
    pub(crate) fn service_input_op(&mut self, op: &InputOp) -> ControlReply {
        self.service_input_payload(op, None)
    }

    fn service_targeted_input(&mut self, id: u64, generation: u64, raise: bool, op: &InputOp) -> ControlReply {
        let refusal = |reason| ControlReply::refused("target_unfocusable", json!({
            "id": id, "generation": generation, "reason": reason,
        }));
        let object = match self.resolve_window_target(id, Some(generation)) {
            Ok(object) => object,
            Err(window_control::WindowTargetError::NotMapped) => return refusal("unmapped"),
            Err(error) => return ControlReply::WindowTarget { id, error },
        };
        let record = &self.surfaces[&object];
        let reason = if self.session_lock_active() {
            Some("session_lock")
        } else if self.highest_exclusive_layer().is_some() {
            Some("exclusive_layer")
        } else if record.minimized {
            Some("minimized")
        } else if !workspaces::on_workspace(record, self.workspace_current()) {
            Some("other_workspace")
        } else if !self.surface_is_input_presentable(record) {
            Some("not_presentable")
        } else if !record.layout.visible {
            Some("not_visible")
        } else if self.keyboard.is_grabbed() || self.seat.input_method().keyboard_grabbed() {
            Some("keyboard_grab")
        } else if matches!(op, InputOp::PointerButton { .. })
            && (self.chrome_pointer_grab.is_some() || self.interactive_pointer.is_some()
                || (self.pointer.is_grabbed() && self.delivery_target(false) != Some((id, generation)))) {
            Some("pointer_grab")
        } else { None };
        if let Some(reason) = reason {
            return refusal(reason);
        }
        let reply = self.service_window_focus(id, generation, raise);
        if !matches!(&reply, ControlReply::Body(body) if body["focused"] == true)
            || self.delivery_target(true) != Some((id, generation)) {
            return refusal("focus_refused");
        }
        // There is no event-loop yield between the fence, focus and injection.
        self.service_input_payload(op, Some((id, generation)))
    }

    fn service_input_payload(&mut self, op: &InputOp, target_window: Option<(u64, u64)>) -> ControlReply {
        if matches!(op, InputOp::Key { .. } | InputOp::Text(_))
            && self.keyboard.current_focus().and_then(|target| target.owned_surface()).is_none() {
            return ControlReply::refused("no_keyboard_target", json!({}));
        }
        let injected_at_us = monotonic_micros();
        let time = (injected_at_us / 1_000) as u32;
        let (kind, keyboard) = match op {
            InputOp::Targeted { id, generation, raise, op } => {
                return self.service_targeted_input(*id, *generation, *raise, op);
            }
            InputOp::PointerMove { target, corners } => {
                let input = match self.pointer_move_input(target, time) {
                    Ok(input) => input,
                    Err(reply) => return reply,
                };
                self.injection.suppress_corners = !corners;
                self.inject(input);
                self.injection.suppress_corners = false;
                (InjectedKind::PointerMove, false)
            }
            InputOp::PointerButton { button, action } => {
                if let Some((id, generation)) = target_window && !self.pointer.is_grabbed() {
                    let object = match self.resolve_window_target(id, Some(generation)) {
                        Ok(object) => object,
                        Err(error) => return ControlReply::WindowTarget { id, error },
                    };
                    let record = &self.surfaces[&object];
                    let surface = record.role.wl_surface().clone();
                    let origin = (f64::from(record.layout.x), f64::from(record.layout.y)).into();
                    let focus = Some((self.seat_focus_target_for(&surface), origin));
                    let pointer = self.pointer.clone();
                    let location = self.cursor_position;
                    self.injection.events = self.injection.events.wrapping_add(1);
                    pointer.motion(self, focus.clone(), &MotionEvent {
                        location: location.into(),
                        serial: SERIAL_COUNTER.next_serial(),
                        time,
                    });
                    pointer.frame(self);
                    self.record_pointer_focus_local_position(focus.as_ref(), location);
                }
                for state in press_states(*action) {
                    let input = HostInput::PointerButton {
                        button: *button,
                        state: *state,
                        time,
                    };
                    if target_window.is_some() {
                        // The focus verb has already applied the requested
                        // raise policy. A device click would hit-test again
                        // and unconditionally raise, defeating raise:false.
                        self.injection.held.note(self.injection.current_run, &input);
                        self.injection.events = self.injection.events.wrapping_add(1);
                        self.notify_idle_activity();
                        let pointer = self.pointer.clone();
                        pointer.button(self, &ButtonEvent {
                            serial: SERIAL_COUNTER.next_serial(), time, button: *button,
                            state: smithay_button_state(*state),
                        });
                        pointer.frame(self);
                    } else {
                        self.inject(input);
                    }
                }
                (InjectedKind::PointerButton, false)
            }
            InputOp::PointerScroll {
                dx,
                dy,
                source,
                v120,
            } => {
                let axis = |amount: Option<f64>, v120: Option<i32>| {
                    amount.map(|amount| HostAxis { amount, v120 })
                };
                self.inject(HostInput::PointerAxis {
                    horizontal: axis(*dx, v120.0),
                    vertical: axis(*dy, v120.1),
                    source: match source {
                        ScrollSource::Wheel => AxisSource::Wheel,
                        ScrollSource::Finger => AxisSource::Finger,
                        ScrollSource::Continuous => AxisSource::Continuous,
                    },
                    relative_direction: (
                        AxisRelativeDirection::Identical,
                        AxisRelativeDirection::Identical,
                    ),
                    time,
                });
                (InjectedKind::PointerScroll, false)
            }
            InputOp::Key {
                key,
                action,
                modifiers,
            } => {
                let index = self.keymap_index();
                let Some((keycode, shifted)) = self.resolve_key(&index, key) else {
                    return unknown_key(key);
                };
                let mut held = Vec::with_capacity(modifiers.len() + 1);
                for modifier in modifiers {
                    let Some((modifier, _)) = self.resolve_key(&index, modifier) else {
                        return unknown_key(modifier);
                    };
                    held.push(modifier);
                }
                if shifted {
                    let shift = KeySpec::Name("Shift_L".into());
                    let Some((shift, _)) = self.resolve_key(&index, &shift) else {
                        return unknown_key(&shift);
                    };
                    if !held.contains(&shift) {
                        held.push(shift);
                    }
                }
                if *action != PressAction::Release {
                    for modifier in &held {
                        self.inject_key(*modifier, HostButtonState::Pressed, time);
                    }
                }
                for state in press_states(*action) {
                    self.inject_key(keycode, *state, time);
                }
                if *action != PressAction::Press {
                    for modifier in held.iter().rev() {
                        self.inject_key(*modifier, HostButtonState::Released, time);
                    }
                }
                (InjectedKind::Key, true)
            }
            InputOp::Text(text) => {
                // An input method holding the keyboard would compose the
                // keys into something else; typed text must arrive as sent.
                if self.seat.input_method().keyboard_grabbed() {
                    return ControlReply::refused("ime_active", json!({}));
                }
                let index = self.keymap_index();
                let shift = self.resolve_key(&index, &KeySpec::Name("Shift_L".into()));
                let mut keys = Vec::with_capacity(text.len());
                for (position, character) in text.chars().enumerate() {
                    // XKB's Return keysym reads back as carriage return.
                    let lookup = if character == '\n' { '\r' } else { character };
                    match index.by_char.get(&u32::from(lookup)) {
                        Some((keycode, false)) => keys.push((*keycode, None)),
                        Some((keycode, true)) if shift.is_some() => {
                            keys.push((*keycode, shift.map(|(shift, _)| shift)));
                        }
                        _ => {
                            return ControlReply::refused(
                                "unmappable",
                                json!({"char": character.to_string(), "index": position}),
                            );
                        }
                    }
                }
                for (keycode, shift) in keys {
                    if let Some(shift) = shift {
                        self.inject_key(shift, HostButtonState::Pressed, time);
                    }
                    self.inject_key(keycode, HostButtonState::Pressed, time);
                    self.inject_key(keycode, HostButtonState::Released, time);
                    if let Some(shift) = shift {
                        self.inject_key(shift, HostButtonState::Released, time);
                    }
                }
                (InjectedKind::Text, true)
            }
            InputOp::ReleaseAll => {
                self.release_injected(time);
                (InjectedKind::ReleaseAll, false)
            }
        };
        // The one mint for `input_seq`: never taken from a caller, so the
        // stats registry's strictly-increasing rule holds by construction.
        let input_seq = self.injection.next_seq.saturating_add(1);
        self.injection.next_seq = input_seq;
        let target = target_window.or_else(|| self.delivery_target(keyboard));
        self.note_injected_input(
            target.map(|(id, _)| SurfaceId(id)),
            InputMark {
                input_seq,
                injected_at_us,
            },
        );
        crate::frame_trace::event("comp_input_injected", || {
            (input_seq, kind as u64, target.map_or(0, |(id, _)| id))
        });
        let pointer = self.pointer_output_position();
        ControlReply::Body(json!({
            "input_seq": input_seq,
            "injected_at_us": injected_at_us,
            "pointer": pointer.map(|(output, x, y)| json!({"output": output, "x": x, "y": y})),
            "target": target.map(|(id, generation)| json!({"id": id, "generation": generation})),
        }))
    }

    /// `release_all`: release everything injection holds, and only that.
    fn release_injected(&mut self, time: u32) {
        let holds = std::mem::take(&mut self.injection.held);
        self.release_holds(holds.owners.into_keys().collect(), time);
    }

    /// Release the given holds the seat still has pressed: keys (newest
    /// code first), then buttons.
    fn release_holds(&mut self, holds: Vec<Hold>, time: u32) {
        let pressed = self.keyboard.pressed_keys();
        for hold in holds.iter().rev() {
            let Hold::Key(raw) = *hold else { continue };
            let keycode = Keycode::new(raw);
            if pressed.contains(&keycode) {
                self.inject_key(keycode, HostButtonState::Released, time);
            }
        }
        let pressed = self.pointer.current_pressed();
        for hold in holds {
            let Hold::Button(button) = hold else { continue };
            if pressed.contains(&button) {
                self.inject(HostInput::PointerButton {
                    button,
                    state: HostButtonState::Released,
                    time,
                });
            }
        }
    }

    fn pointer_move_input(
        &self,
        target: &PointerMoveTarget,
        time: u32,
    ) -> Result<HostInput, ControlReply> {
        match target {
            PointerMoveTarget::Relative { dx, dy } => Ok(HostInput::PointerMotion {
                dx: *dx,
                dy: *dy,
                dx_unaccel: *dx,
                dy_unaccel: *dy,
                time,
            }),
            PointerMoveTarget::Output { output, x, y } => {
                let row = match output {
                    Some(requested) => {
                        let projection = project_outputs(self).ok_or(ControlReply::Busy)?;
                        projection
                            .rows
                            .into_iter()
                            .find(|(key, row)| key == requested || row.name == *requested)
                    }
                    None => self
                        .backend
                        .default_output()
                        .and_then(|output| project_output(self, &output)),
                };
                let Some((key, row)) = row else {
                    return Err(ControlReply::refused(
                        "unknown_output",
                        json!({"output": output}),
                    ));
                };
                let (width, height) = (f64::from(row.width), f64::from(row.height));
                if !(0.0..width).contains(x) || !(0.0..height).contains(y) {
                    return Err(ControlReply::refused(
                        "out_of_bounds",
                        json!({"output": key, "x": x, "y": y, "width": row.width, "height": row.height}),
                    ));
                }
                Ok(HostInput::PointerMotionAbsolute {
                    x: f64::from(row.x) + x,
                    y: f64::from(row.y) + y,
                    time,
                })
            }
            PointerMoveTarget::Window {
                id,
                generation,
                x,
                y,
                require_hit,
            } => {
                let object = self
                    .resolve_window_target(*id, Some(*generation))
                    .map_err(|error| ControlReply::WindowTarget { id: *id, error })?;
                let origin = self.surfaces[&object].window_origin;
                let (global_x, global_y) = (f64::from(origin.0) + x, f64::from(origin.1) + y);
                if *require_hit {
                    let on_output = project_outputs(self).is_some_and(|projection| {
                        projection.rows.values().any(|row| {
                            (f64::from(row.x)..f64::from(row.x) + f64::from(row.width))
                                .contains(&global_x)
                                && (f64::from(row.y)..f64::from(row.y) + f64::from(row.height))
                                    .contains(&global_y)
                        })
                    });
                    if !on_output {
                        return Err(ControlReply::refused(
                            "off_output",
                            json!({"id": id, "x": x, "y": y}),
                        ));
                    }
                    let under = self.root_record_at(global_x, global_y);
                    if under.as_ref().map(|(under, _, _)| under) != Some(&object) {
                        return Err(ControlReply::refused(
                            "occluded",
                            json!({
                                "id": id,
                                "under": under.map(|(_, id, generation)| {
                                    json!({"id": id, "generation": generation})
                                }),
                            }),
                        ));
                    }
                }
                Ok(HostInput::PointerMotionAbsolute {
                    x: global_x,
                    y: global_y,
                    time,
                })
            }
        }
    }

    /// The root window record a pointer at this point would reach (client
    /// content or its compositor chrome).
    fn root_record_at(&self, x: f64, y: f64) -> Option<(ObjectId, u64, u64)> {
        let object = match self.pointer_target_at(x, y)? {
            PointerTarget::Client { surface, .. } => {
                canonical_root_surface(&self.popup_manager, &surface).id()
            }
            PointerTarget::Chrome { object, .. } => object,
        };
        let record = self.surfaces.get(&object)?;
        Some((object, record.id.0, record.generation))
    }

    /// `{id, generation}` of the root of whatever the seat now delivers to:
    /// keyboard focus for key verbs, pointer focus otherwise.
    fn delivery_target(&self, keyboard: bool) -> Option<(u64, u64)> {
        let surface = if keyboard {
            self.keyboard
                .current_focus()
                .and_then(|target| target.owned_surface())
        } else {
            self.pointer
                .current_focus()
                .and_then(|target| target.owned_surface())
        }?;
        let root = canonical_root_surface(&self.popup_manager, &surface);
        self.surfaces
            .get(&root.id())
            .map(|record| (record.id.0, record.generation))
    }

    /// The cursor as `(output key, output-local x, y)`.
    fn pointer_output_position(&self) -> Option<(String, f64, f64)> {
        let (x, y) = self.cursor_position;
        project_outputs(self)?
            .rows
            .into_iter()
            .find_map(|(key, row)| {
                let local = (x - f64::from(row.x), y - f64::from(row.y));
                ((0.0..f64::from(row.width)).contains(&local.0)
                    && (0.0..f64::from(row.height)).contains(&local.1))
                .then_some((key, local.0, local.1))
            })
    }

    pub(crate) fn host_passthrough_available(&self) -> bool {
        matches!(self.backend, BackendData::Winit(_))
    }

    pub(crate) fn host_passthrough(&self) -> bool {
        self.injection.host_passthrough
    }

    pub(crate) fn set_host_passthrough(&mut self, passthrough: bool) {
        self.injection.host_passthrough = passthrough;
    }

    pub(crate) fn host_input_snapshot(&self) -> Option<HostInputSnapshot> {
        self.host_passthrough_available()
            .then_some(HostInputSnapshot {
                passthrough: self.injection.host_passthrough,
            })
    }

    /// `input.host.passthrough = false`: drop host pointer and key input
    /// (resize, scale, pointer leave and touch still pass). A host key or
    /// button pressed while passthrough was on still gets its release, and
    /// a host focus loss releases only those, never an injected hold.
    pub(super) fn filter_host_passthrough(&mut self, inputs: Vec<HostInput>) -> Vec<HostInput> {
        let injection = &mut self.injection;
        let open = injection.host_passthrough;
        let mut passed = Vec::with_capacity(inputs.len());
        for input in inputs {
            match input {
                HostInput::Key { keycode, state, .. } => {
                    let pass = match state {
                        HostButtonState::Pressed => {
                            open && {
                                injection.host_held_keys.insert(keycode.raw());
                                true
                            }
                        }
                        HostButtonState::Released => {
                            injection.host_held_keys.remove(&keycode.raw()) || open
                        }
                    };
                    if pass {
                        passed.push(input);
                    }
                }
                HostInput::PointerButton { button, state, .. } => {
                    let pass = match state {
                        HostButtonState::Pressed => {
                            open && {
                                injection.host_held_buttons.insert(button);
                                true
                            }
                        }
                        HostButtonState::Released => {
                            injection.host_held_buttons.remove(&button) || open
                        }
                    };
                    if pass {
                        passed.push(input);
                    }
                }
                HostInput::PointerMotionAbsolute { .. }
                | HostInput::PointerMotion { .. }
                | HostInput::PointerAxis { .. } => {
                    if open {
                        passed.push(input);
                    }
                }
                HostInput::KeyboardFocusLost => {
                    let held = std::mem::take(&mut injection.host_held_keys);
                    if open {
                        passed.push(input);
                    } else {
                        // Release only the host-held keys, then the rest of
                        // the focus-loss reset (chrome grab, hover, cursor)
                        // without touching an injected hold.
                        let time = monotonic_millis();
                        passed.extend(held.into_iter().map(|raw| HostInput::Key {
                            keycode: Keycode::new(raw),
                            state: HostButtonState::Released,
                            time,
                        }));
                        passed.push(HostInput::KeyboardFocusLostKeepingKeys);
                    }
                }
                _ => passed.push(input),
            }
        }
        passed
    }

    /// Take ownership of a long verb's reply and start it. `admitted` is
    /// when the worker admitted it: deadlines run from there, so the reply
    /// is due when the caller's own budget says.
    pub(crate) fn start_long_op(
        &mut self,
        op: LongOp,
        reply: tokio::sync::oneshot::Sender<ControlReply>,
        admitted: Instant,
    ) {
        match op {
            LongOp::Sequence(steps) => {
                let id = self.injection.next_sequence;
                self.injection.next_sequence = id.wrapping_add(1);
                self.injection.sequences.insert(
                    id,
                    SequenceRun {
                        steps: steps.into(),
                        index: 0,
                        delay_elapsed: false,
                        replies: Vec::new(),
                        started: admitted,
                        reply,
                    },
                );
                self.advance_sequence(id);
            }
            LongOp::Wait(spec) => self.start_window_wait(spec, reply, admitted),
            LongOp::RegionSelect { output, timeout } => {
                self.start_region_selection(output, timeout, reply, admitted)
            }
            LongOp::ForceClose {
                id,
                generation,
                timeout,
            } => self.start_force_close(id, generation, timeout, reply, admitted),
        }
    }

    /// End a run early: give up its holds, and release the ones no other
    /// owner (another run, a single verb) still holds.
    fn abort_sequence(&mut self, id: u64) -> Option<SequenceRun> {
        let run = self.injection.sequences.remove(&id)?;
        let orphaned = self.injection.held.drop_owner(Some(id));
        self.release_holds(orphaned, monotonic_millis());
        Some(run)
    }

    fn arm_sequence_timer(&mut self, id: u64, delay: Duration, elapses_delay: bool) {
        let armed = self.capture_loop_handle.insert_source(
            Timer::from_duration(delay),
            move |_, _, state| {
                if elapses_delay && let Some(run) = state.injection.sequences.get_mut(&id) {
                    run.delay_elapsed = true;
                }
                state.advance_sequence(id);
                TimeoutAction::Drop
            },
        );
        if let Err(error) = armed {
            tracing::warn!(%error, "input sequence timer unavailable");
            if let Some(run) = self.abort_sequence(id) {
                let _ = run.reply.send(ControlReply::Busy);
            }
        }
    }

    /// Run a sequence's due steps. A delayed step arms one calloop timer
    /// and resumes from it; a long zero-delay stretch yields to the loop
    /// every [`SEQUENCE_YIELD_EVENTS`] events. A failed step ends the run
    /// and releases what this run holds, so an aborted drag never leaves a
    /// button down (and never lets go of another caller's hold).
    pub(super) fn advance_sequence(&mut self, id: u64) {
        let events_at_start = self.injection.events;
        loop {
            let Some(run) = self.injection.sequences.get_mut(&id) else {
                return;
            };
            if run.reply.is_closed() {
                // The caller stopped waiting; do not keep driving the seat
                // for nobody.
                self.abort_sequence(id);
                return;
            }
            let Some(step) = run.steps.front() else {
                let run = self
                    .injection
                    .sequences
                    .remove(&id)
                    .expect("sequence run present");
                let elapsed_ms =
                    u64::try_from(run.started.elapsed().as_millis()).unwrap_or(u64::MAX);
                let _ = run.reply.send(ControlReply::Body(json!({
                    "steps": run.replies,
                    "elapsed_ms": elapsed_ms,
                })));
                return;
            };
            if !step.delay.is_zero() && !run.delay_elapsed {
                let delay = step.delay;
                self.arm_sequence_timer(id, delay, true);
                return;
            }
            if self.injection.events.wrapping_sub(events_at_start) >= SEQUENCE_YIELD_EVENTS {
                self.arm_sequence_timer(id, SEQUENCE_YIELD, false);
                return;
            }
            let step = run.steps.pop_front().expect("front step present");
            let index = run.index;
            run.index += 1;
            run.delay_elapsed = false;
            self.injection.current_run = Some(id);
            let reply = self.service_input_op(&step.op);
            self.injection.current_run = None;
            match reply {
                ControlReply::Body(body) => {
                    if let Some(run) = self.injection.sequences.get_mut(&id) {
                        run.replies.push(body);
                    }
                }
                refusal => {
                    let run = self.abort_sequence(id).expect("sequence run present");
                    let _ = run.reply.send(ControlReply::refused(
                        "step_failed",
                        json!({
                            "index": index,
                            "verb": step.verb,
                            "step": refusal.wire_json(),
                            "completed": run.replies,
                            "released": true,
                        }),
                    ));
                    return;
                }
            }
        }
    }
}

fn unknown_key(key: &KeySpec) -> ControlReply {
    ControlReply::refused(
        "unknown_key",
        match key {
            KeySpec::Name(name) => json!({"key": name}),
            KeySpec::Evdev(code) => json!({"key": code}),
        },
    )
}
