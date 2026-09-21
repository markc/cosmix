//! One modal seat operation. There is no caller ownership/permission policy.
use super::*;
use crate::{
    occlusion::OutputGeometry,
    port::ControlReply,
    region_scene::{RegionBridge, View},
};
use serde_json::json;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::wayland::input_method::InputMethodSeat as _;

pub(crate) const REGION_CLEANUP_BUDGET: Duration = Duration::from_secs(3);

#[derive(Default)]
pub(super) struct RegionSelection {
    pub bridge: Option<RegionBridge>,
    pub suspended: bool,
    pub touches: HashSet<TouchSlot>,
    suppressed_touches: HashSet<TouchSlot>,
    keys: HashSet<Keycode>,
    buttons: HashSet<u32>,
    next_id: u64,
    run: Option<Run>,
}

struct Run {
    view: View,
    requested: Option<String>,
    focus: Option<SeatFocusTarget>,
    cursor: CursorSelection,
    deadline: Instant,
    reply_deadline: Instant,
    reply: tokio::sync::oneshot::Sender<ControlReply>,
    result: Option<ControlReply>,
}

/// Displayed output-local logical integer rectangle, rounded outwards.
/// A click or a line is not a region, even at fractional coordinates.
pub(super) fn normalise(output: &OutputGeometry, a: (f64, f64), b: (f64, f64)) -> Option<[i32; 4]> {
    if ![a.0, a.1, b.0, b.1].iter().all(|v| v.is_finite()) {
        return None;
    }
    let bounds = output.bounds;
    let (ax, ay) = (
        (a.0 - bounds.x).clamp(0., bounds.w),
        (a.1 - bounds.y).clamp(0., bounds.h),
    );
    let (bx, by) = (
        (b.0 - bounds.x).clamp(0., bounds.w),
        (b.1 - bounds.y).clamp(0., bounds.h),
    );
    if ax == bx || ay == by {
        return None;
    }
    let (x, y, r, b) = (
        ax.min(bx).floor(),
        ay.min(by).floor(),
        ax.max(bx).ceil().min(bounds.w),
        ay.max(by).ceil().min(bounds.h),
    );
    if r > i32::MAX as f64 || b > i32::MAX as f64 {
        return None;
    }
    let rect = [x as i32, y as i32, (r - x) as i32, (b - y) as i32];
    (rect[2] > 0 && rect[3] > 0).then_some(rect)
}

fn refused(error: &'static str) -> ControlReply {
    ControlReply::Refused {
        error,
        detail: json!({}),
    }
}

impl WaylandState {
    pub(super) fn start_region_selection(
        &mut self,
        output: Option<String>,
        timeout: Duration,
        reply: tokio::sync::oneshot::Sender<ControlReply>,
        admitted: Instant,
    ) {
        let busy = self.region.run.is_some()
            || self.region.bridge.is_none()
            || !self.region.buttons.is_empty()
            || !self.region.keys.is_empty()
            || self.pointer.is_grabbed()
            || self.keyboard.is_grabbed()
            || self.seat.input_method().keyboard_grabbed()
            || !self.pointer.current_pressed().is_empty()
            || self.chrome_pointer_grab.is_some()
            || self.chrome_pressed.is_some()
            || self.interactive_pointer.is_some()
            || !self.region.touches.is_empty()
            || !self.injection.sequences.is_empty()
            || self.seat.get_touch().is_some_and(|t| t.is_grabbed());
        #[cfg(feature = "embedded-quoin")]
        let busy = busy || self.embedded_shell.as_ref().is_some_and(|b| b.held());
        if busy {
            let _ = reply.send(ControlReply::Busy);
            return;
        }
        if self.session_lock_active() {
            let _ = reply.send(ControlReply::Locked);
            return;
        }
        let available = self.backend.occlusion_outputs();
        if output
            .as_ref()
            .is_some_and(|name| !available.iter().any(|o| o.name == *name))
        {
            let _ = reply.send(refused("unknown_output"));
            return;
        }
        let outputs = available
            .into_iter()
            .filter(|o| o.generation != 0 && output.as_ref().is_none_or(|name| *name == o.name))
            .collect::<Vec<_>>();
        if outputs.is_empty() {
            let _ = reply.send(refused("output_changed"));
            return;
        }
        if Instant::now() >= admitted + timeout {
            let _ = reply.send(ControlReply::Body(json!({"version":1,"status":"timeout"})));
            return;
        }
        self.region.next_id += 1;
        let view = View {
            id: self.region.next_id,
            active: true,
            outputs,
            selected: output.clone(),
            start: None,
            pointer: self.cursor_position,
        };
        let run = Run {
            view,
            requested: output,
            focus: self.keyboard.current_focus(),
            cursor: self.cursor_selection.clone(),
            deadline: admitted + timeout,
            // Three seconds to remove and submit furniture; LongOp reserves four.
            // Thus even failure replies precede both the 60s cap and its waiter.
            reply_deadline: admitted + timeout + REGION_CLEANUP_BUDGET,
            reply,
            result: None,
        };
        self.region.suspended = true;
        self.region.run = Some(run);
        self.break_pointer_constraint_for_corner();
        self.reset_corner_detector();
        self.update_chrome_hover(None);
        self.titlebar_click_candidate = None;
        self.last_keyboard_action = None;
        self.chrome_cursor_override = None;
        #[cfg(feature = "embedded-quoin")]
        if let Some(bridge) = &self.embedded_shell {
            bridge.suspend(true);
        }
        // Unlike session lock, leave focus before retiring presses: wl_keyboard.leave
        // tells the client to release all keys. Smithay still updates its internal
        // pressed set with no focus, without forwarding releases to that client.
        self.keyboard
            .clone()
            .set_focus(self, None, SERIAL_COUNTER.next_serial());
        // Retire pre-existing forwarded presses while focus is None. Merely
        // intercepting later releases leaves Smithay's forwarded set stale.
        self.region.keys.extend(self.keyboard.pressed_keys());
        self.release_pressed_keys();
        self.pointer.clone().motion(
            self,
            None,
            &MotionEvent {
                location: self.cursor_position.into(),
                serial: SERIAL_COUNTER.next_serial(),
                time: monotonic_millis(),
            },
        );
        self.pointer.clone().frame(self);
        self.pointer_focus_local_position = None;
        self.pending_relative_motion = None;
        self.cursor_selection = CursorSelection::Hidden;
        self.publish_current_cursor();
        self.publish_region_view();
        let id = self.region.next_id;
        if self
            .capture_loop_handle
            .insert_source(
                Timer::from_duration(Duration::from_millis(10)),
                move |_, _, state| {
                    if state.region.run.as_ref().is_none_or(|r| r.view.id != id) {
                        return TimeoutAction::Drop;
                    }
                    state.poll_region_selection(Instant::now());
                    if state.region.run.is_some() {
                        TimeoutAction::ToDuration(Duration::from_millis(10))
                    } else {
                        TimeoutAction::Drop
                    }
                },
            )
            .is_err()
        {
            self.finish_region_selection(ControlReply::Busy);
            self.complete_region_selection(Some(ControlReply::Busy));
        }
    }

    fn publish_region_view(&self) {
        if let (Some(bridge), Some(run)) = (&self.region.bridge, &self.region.run) {
            bridge.set(run.view.clone());
        }
    }

    fn region_outputs_current(&self) -> bool {
        let current = self.backend.occlusion_outputs();
        self.region
            .run
            .as_ref()
            .is_some_and(|run| run.view.outputs.iter().all(|o| current.contains(o)))
    }

    pub(super) fn poll_region_selection(&mut self, now: Instant) {
        if self.region.run.is_none() {
            return;
        }
        if !self.region_outputs_current() {
            self.finish_region_selection(refused("output_changed"));
            // An undecided run has now replied with the refusal. Only an
            // already-selected result remains to wait for removal evidence.
            if self.region.run.is_none() {
                return;
            }
            let current = self.backend.occlusion_outputs();
            if let Some(run) = &mut self.region.run {
                // Removed/inactive outputs cannot submit another frame. Require
                // clean frames from surviving ready outputs, including a new
                // generation of the same connector, after entity removal.
                run.view.outputs = current
                    .into_iter()
                    .filter(|o| {
                        o.generation != 0 && run.view.outputs.iter().any(|old| old.name == o.name)
                    })
                    .collect();
            }
            self.publish_region_view();
        }
        let Some(run) = &self.region.run else {
            return;
        };
        if run.reply.is_closed() {
            self.finish_region_selection(ControlReply::Busy);
            self.complete_region_selection(Some(ControlReply::Busy));
            return;
        }
        if run.result.is_none() && now >= run.deadline {
            self.finish_region_selection(ControlReply::Body(
                json!({"version":1,"status":"timeout"}),
            ));
        }
        let Some(run) = &self.region.run else {
            return;
        };
        if run.result.is_some()
            && self
                .region
                .bridge
                .as_ref()
                .is_some_and(|b| b.clean(run.view.id))
        {
            self.complete_region_selection(None);
        } else if now >= run.reply_deadline {
            // Never claim selection succeeded without a clean submitted frame.
            self.finish_region_selection(ControlReply::Busy);
            self.complete_region_selection(Some(ControlReply::Busy));
        }
    }

    pub(super) fn finish_region_selection(&mut self, result: ControlReply) {
        let Some(run) = &mut self.region.run else {
            return;
        };
        if run.result.is_some() {
            return;
        }
        let needs_clean_frame = matches!(&result,
            ControlReply::Body(body) if body["status"] == "selected");
        run.result = Some(result);
        run.view.active = false;
        self.publish_region_view();
        self.restore_region_focus();
        if !needs_clean_frame {
            // Only a selected rectangle authorises capture. Other replies need
            // focus restored and removal published, not presentation evidence.
            self.complete_region_selection(None);
        }
    }

    pub(super) fn abandon_region_input(&mut self) {
        if self.region.run.is_none()
            && self.region.keys.is_empty()
            && self.region.buttons.is_empty()
            && self.region.touches.is_empty()
            && self.region.suppressed_touches.is_empty()
        {
            return;
        }
        self.finish_region_selection(ControlReply::Busy);
        let held = self.keyboard.pressed_keys();
        for key in std::mem::take(&mut self.region.keys) {
            if held.contains(&key) {
                self.region_key(key, HostButtonState::Released, monotonic_millis());
            }
        }
        self.region.buttons.clear();
        self.region.touches.clear();
        self.region.suppressed_touches.clear();
    }

    fn restore_region_focus(&mut self) {
        if !self.region.suspended {
            return;
        }
        let Some(run) = &self.region.run else {
            return;
        };
        let focus = run.focus.clone().filter(|target| {
            target.owned_surface().is_some_and(|s| {
                self.surfaces.get(&s.id()).is_some_and(|r| {
                    surface_is_presentable(r)
                        && r.layout.visible
                        && !r.minimized
                        && self.surface_is_input_presentable(r)
                })
            })
        });
        self.cursor_selection = run.cursor.clone();
        self.pending_relative_motion = None;
        // This still invokes clipboard/text-input hooks, but suspension prevents
        // activation/configure/restacking changes in SeatHandler::focus_changed.
        self.keyboard
            .clone()
            .set_focus(self, focus, SERIAL_COUNTER.next_serial());
        self.region.suspended = false;
        #[cfg(feature = "embedded-quoin")]
        if let Some(bridge) = &self.embedded_shell {
            bridge.suspend(false);
        }
        self.arbitrate_keyboard_focus(None, self.keyboard.current_focus().is_none(), false);
        // The saved surface may have been unmapped/minimised while suspended.
        // Even None -> None must now reconcile stale desktop activation flags.
        let seat = self.seat.clone();
        let focused = self.keyboard.current_focus();
        self.focus_changed(&seat, focused.as_ref());
        self.retarget_pointer_after_visibility_change();
        self.service_deferred_constraint_activation();
        self.publish_current_cursor();
    }

    fn complete_region_selection(&mut self, override_result: Option<ControlReply>) {
        self.restore_region_focus();
        if let Some(run) = self.region.run.take() {
            // A disappeared waiter needs no further delivery; cleanup already ran.
            let _ = run
                .reply
                .send(override_result.or(run.result).unwrap_or(ControlReply::Busy));
        }
    }

    pub(super) fn region_input(&mut self, input: &HostInput) -> bool {
        // Recovery also owns release tails after the modal operation has ended.
        if matches!(
            input,
            HostInput::KeyboardFocusLost
                | HostInput::KeyboardFocusLostKeepingKeys
                | HostInput::PointerLeave
                | HostInput::TouchDeviceRemoved
        ) {
            self.abandon_region_input();
            return false;
        }
        match input {
            HostInput::TouchDown { slot, .. } => {
                self.region.touches.insert(*slot);
            }
            HostInput::TouchUp { slot, .. } => {
                self.region.touches.remove(slot);
            }
            HostInput::TouchCancel => self.region.touches.clear(),
            _ => {}
        }
        match input {
            HostInput::TouchDown { slot, .. }
                if self.region.suspended || !self.region.suppressed_touches.is_empty() =>
            {
                self.region.suppressed_touches.insert(*slot);
                if !self.region.suspended {
                    return true;
                }
            }
            HostInput::TouchUp { slot, .. } if self.region.suppressed_touches.remove(slot) => {
                return true;
            }
            HostInput::TouchMotion { slot, .. }
                if self.region.suppressed_touches.contains(slot) =>
            {
                return true;
            }
            HostInput::TouchCancel => self.region.suppressed_touches.clear(),
            _ => {}
        }
        // Tail releases remain intercepted after timeout/Esc/right-click. XKB
        // still sees key releases; selection buttons never entered Smithay.
        if let HostInput::Key {
            keycode,
            state: HostButtonState::Released,
            time,
        } = *input
            && self.region.keys.remove(&keycode)
        {
            self.region_key(keycode, HostButtonState::Released, time);
            return true;
        }
        if let HostInput::PointerButton {
            button,
            state: HostButtonState::Released,
            ..
        } = *input
            && !self.region.suspended
            && self.region.buttons.remove(&button)
        {
            return true;
        }
        if !self.region.suspended {
            return false;
        }
        self.poll_region_selection(Instant::now());
        if !self.region.suspended {
            return self.region_input(input);
        }
        match *input {
            HostInput::PointerMotionAbsolute { x, y, .. } => self.region_motion((x, y)),
            HostInput::PointerMotion { dx, dy, .. } => {
                self.region_motion((self.cursor_position.0 + dx, self.cursor_position.1 + dy))
            }
            HostInput::PointerButton { button, state, .. } => {
                if state == HostButtonState::Pressed {
                    self.region.buttons.insert(button);
                } else {
                    self.region.buttons.remove(&button);
                }
                if button == 0x111 && state == HostButtonState::Pressed {
                    self.finish_region_selection(ControlReply::Body(
                        json!({"version":1,"status":"cancelled","reason":"right_button"}),
                    ));
                } else if button == 0x110 {
                    let p = self.cursor_position;
                    let run = self.region.run.as_mut().expect("active selection");
                    if state == HostButtonState::Pressed {
                        let target = run.view.outputs.iter().find(|o| {
                            let b = o.bounds;
                            p.0 >= b.x && p.0 < b.x + b.w && p.1 >= b.y && p.1 < b.y + b.h
                        });
                        if let Some(output) = target {
                            run.view.selected = Some(output.name.clone());
                            run.view.start = Some(p);
                        }
                        self.publish_region_view();
                    } else if let Some(start) = run.view.start {
                        let output = run
                            .view
                            .outputs
                            .iter()
                            .find(|o| Some(&o.name) == run.view.selected.as_ref())
                            .expect("selected output");
                        if let Some([x, y, width, height]) = normalise(output, start, p) {
                            let result = ControlReply::Body(
                                json!({"version":1,"status":"selected",
                                "output":output.name,"output_generation":output.generation,
                                "coordinate_space":"output-local-logical","region":{"x":x,"y":y,"width":width,"height":height}}),
                            );
                            self.finish_region_selection(result);
                        } else {
                            // A click/line keeps the operation armed for another drag.
                            run.view.start = None;
                            run.view.selected = run.requested.clone();
                            self.publish_region_view();
                        }
                    }
                }
            }
            HostInput::Key {
                keycode,
                state,
                time,
            } => {
                if state == HostButtonState::Pressed {
                    self.region.keys.insert(keycode);
                }
                self.region_key(keycode, state, time);
                if keycode.raw() == 9 && state == HostButtonState::Pressed {
                    self.finish_region_selection(ControlReply::Body(
                        json!({"version":1,"status":"cancelled","reason":"escape"}),
                    ));
                }
            }
            HostInput::OutputResized { .. }
            | HostInput::OutputScaleChanged { .. }
            | HostInput::TouchDeviceAdded => return false,
            _ => {} // Scroll/touch cannot reach clients or native furniture during selection.
        }
        true
    }

    fn region_motion(&mut self, point: (f64, f64)) {
        let point = clamp_point_to_seat(point, &self.backend.seat_regions()).position;
        self.cursor_position = point;
        let mut snapshot = self
            .cursor_position_snapshot
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        snapshot.x = point.0;
        snapshot.y = point.1;
        snapshot.on_output = true;
        snapshot.revision = snapshot.revision.saturating_add(1);
        drop(snapshot);
        if let Some(run) = &mut self.region.run {
            run.view.pointer = point;
        }
        self.publish_region_view();
    }

    fn region_key(&mut self, keycode: Keycode, state: HostButtonState, time: u32) {
        let pressed = state == HostButtonState::Pressed;
        let action = self
            .keyboard
            .clone()
            .input::<Option<BindingAction>, _>(
                self,
                keycode,
                smithay_key_state(state),
                SERIAL_COUNTER.next_serial(),
                time,
                |state, mods, key| {
                    let disposition = state.bindings.dispatch_session_locked(
                        keycode,
                        pressed,
                        key.raw_latin_sym_or_raw_current_sym(),
                        mods,
                    );
                    FilterResult::Intercept(match disposition {
                        KeyDisposition::Act(action @ BindingAction::SwitchVt(_)) => Some(action),
                        _ => None,
                    })
                },
            )
            .flatten();
        self.last_keyboard_action = None;
        if let Some(action) = action {
            self.finish_region_selection(ControlReply::Busy);
            self.handle_binding_action(action);
        }
    }
}
