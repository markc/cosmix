//! Agent delivery uses independent Smithay device state. It never enters
//! host input arbitration, compositor bindings, constraints or chrome.

use super::*;

impl WaylandState {
    pub(super) fn deliver_agent_input(&mut self, input: HostInput) {
        match input {
            HostInput::Key { keycode, state, time } => {
                let keyboard = self.agent.keyboard.clone();
                let serial = SERIAL_COUNTER.next_serial();
                keyboard.input::<(), _>(self, keycode, smithay_key_state(state), serial, time,
                    |state, _, _| {
                        state.agent.delivery.key_handled = state.agent.keyboard.current_focus().is_some();
                        state.agent.delivery.key_delivery = state.delivery_target_on(SeatKind::Agent, true);
                        smithay::input::keyboard::FilterResult::Forward
                    });
                self.agent.last_keyboard_action = if state == HostButtonState::Pressed {
                    keyboard.current_focus().and_then(|target| target.owned_surface()).map(|surface| {
                        (serial, canonical_root_surface(&self.popup_manager, &surface))
                    })
                } else { None };
            }
            HostInput::PointerButton { button, state, time } => {
                self.agent.delivery.button_delivery = self.delivery_target_on(SeatKind::Agent, false);
                let pointer = self.agent.pointer.clone();
                pointer.button(self, &ButtonEvent {
                    serial: SERIAL_COUNTER.next_serial(), time, button,
                    state: smithay_button_state(state),
                });
                pointer.frame(self);
            }
            HostInput::PointerAxis { horizontal, vertical, source, relative_direction, time } => {
                let mut frame = AxisFrame::new(time).source(source);
                let mut carries_anything = false;
                for (axis, direction, report) in [
                    (Axis::Horizontal, relative_direction.0, horizontal),
                    (Axis::Vertical, relative_direction.1, vertical),
                ] {
                    let Some(report) = report else { continue };
                    let stops = report.amount == 0.0 && matches!(source, AxisSource::Finger | AxisSource::Continuous);
                    if !stops && report.amount == 0.0 && report.v120.is_none_or(|value| value == 0) { continue; }
                    carries_anything = true;
                    frame = frame.relative_direction(axis, direction).value(axis, report.amount);
                    if let Some(value) = report.v120 { frame = frame.v120(axis, value); }
                    if stops { frame = frame.stop(axis); }
                }
                if carries_anything {
                    let pointer = self.agent.pointer.clone();
                    pointer.axis(self, frame);
                    pointer.frame(self);
                }
            }
            // Resolved client-only motion is added by the targeting layer.
            _ => {}
        }
    }
}
