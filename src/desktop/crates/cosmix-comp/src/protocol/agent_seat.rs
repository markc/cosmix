//! Agent delivery uses independent Smithay device state. It never enters
//! host input arbitration, compositor bindings, constraints or chrome.

use super::*;
use crate::port::{ControlReply, InputOp, PointerMoveTarget, PressAction};
use serde_json::json;

impl WaylandState {
    fn agent_refusal(reason: &'static str) -> ControlReply {
        let mut detail = json!({});
        if matches!(
            reason,
            "agent_seat_unbound" | "x11_unsupported" | "chrome_target"
        ) {
            detail["hint"] = json!({"seat":"human"});
        }
        if reason == "agent_seat_unbound" {
            detail["message"] = json!("target client is currently unbound on the agent seat");
        }
        ControlReply::refused(reason, detail)
    }

    fn validate_agent_surface(
        &self,
        surface: &WlSurface,
        keyboard: bool,
    ) -> Result<(), ControlReply> {
        if self.session_lock_active() {
            return Err(Self::agent_refusal("session_lock"));
        }
        let root = canonical_root_surface(&self.popup_manager, surface);
        let record = self
            .surfaces
            .get(&root.id())
            .ok_or_else(|| Self::agent_refusal("unmapped"))?;
        #[cfg(feature = "xwayland")]
        if matches!(record.role, SurfaceRole::X11(_)) {
            return Err(Self::agent_refusal("x11_unsupported"));
        }
        if !record.mapped {
            return Err(Self::agent_refusal("unmapped"));
        }
        if self
            .surfaces
            .get(&surface.id())
            .is_none_or(|record| !self.agent_tree_mapped(record))
        {
            return Err(Self::agent_refusal("unmapped"));
        }
        if !self.surface_is_input_presentable(record) {
            return Err(Self::agent_refusal("not_presentable"));
        }
        let client = surface
            .client()
            .ok_or_else(|| Self::agent_refusal("unmapped"))?;
        let bound = if keyboard {
            self.agent
                .keyboard
                .client_keyboards(&client)
                .next()
                .is_some()
        } else {
            self.agent.pointer.client_pointers(&client).next().is_some()
        };
        if !bound {
            return Err(Self::agent_refusal("agent_seat_unbound"));
        }
        Ok(())
    }

    pub(super) fn agent_preflight(&self, op: &InputOp) -> Result<(), ControlReply> {
        if self.session_lock_active() {
            return Err(Self::agent_refusal("session_lock"));
        }
        if matches!(op, InputOp::ReleaseAll | InputOp::Key { action: PressAction::Release, .. }
            | InputOp::PointerButton { action: PressAction::Release, .. }) {
            return Ok(());
        }
        if matches!(op, InputOp::Key { .. } | InputOp::Text(_))
            && self
                .agent
                .keyboard
                .with_grab(|_, grab| !grab.is::<PopupKeyboardGrab<WaylandState>>())
                .unwrap_or(false)
        {
            return Err(Self::agent_refusal("keyboard_grab"));
        }
        if matches!(
            op,
            InputOp::PointerButton { .. } | InputOp::PointerScroll { .. }
        ) && self
            .agent
            .pointer
            .with_grab(|_, grab| {
                !grab.is::<PopupPointerGrab<WaylandState>>()
                    && !grab.is::<smithay::input::pointer::ClickGrab<WaylandState>>()
            })
            .unwrap_or(false)
        {
            return Err(Self::agent_refusal("pointer_grab"));
        }
        match op {
            InputOp::Key {
                action: PressAction::Release,
                ..
            }
            | InputOp::ReleaseAll => Ok(()),
            InputOp::Key { .. } | InputOp::Text(_) => {
                let surface = self.agent_keyboard_delivery_surface()
                    .ok_or_else(|| Self::agent_refusal("no_keyboard_target"))?;
                self.validate_agent_surface(&surface, true)
            }
            InputOp::PointerButton {
                action: PressAction::Release,
                ..
            } => Ok(()),
            InputOp::PointerButton { .. } | InputOp::PointerScroll { .. } => {
                if let Some(surface) = self
                    .agent
                    .pointer
                    .current_focus()
                    .and_then(|target| target.owned_surface())
                {
                    self.validate_agent_surface(&surface, false)
                } else if self.agent_popup_pointer_grab() {
                    // A press outside a popup is delivered to its grab to dismiss it.
                    Ok(())
                } else {
                    Err(Self::agent_refusal("no_pointer_target"))
                }
            }
            _ => Ok(()),
        }
    }

    fn agent_popup_pointer_grab(&self) -> bool {
        self.agent
            .pointer
            .with_grab(|_, grab| grab.is::<PopupPointerGrab<WaylandState>>())
            .unwrap_or(false)
    }

    pub(super) fn service_agent_targeted_input(
        &mut self,
        id: u64,
        generation: u64,
        raise: bool,
        op: &InputOp,
    ) -> ControlReply {
        if raise {
            return Self::agent_refusal("invalid_argument");
        }
        if self.session_lock_active() {
            return Self::agent_refusal("session_lock");
        }
        // Releases retire the driven seat's hold even if its old target has gone.
        // They never resolve or refocus that target.
        if matches!(op, InputOp::Key { action: PressAction::Release, .. }
            | InputOp::PointerButton { action: PressAction::Release, .. }) {
            return self.service_input_payload(SeatKind::Agent, op, Some((id, generation)));
        }
        let object = match self.resolve_window_target(id, Some(generation)) {
            Ok(object) => object,
            Err(error) => return ControlReply::WindowTarget { id, error },
        };
        let surface = self.surfaces[&object].role.wl_surface().clone();
        let keyboard_op = matches!(op, InputOp::Key { .. } | InputOp::Text(_));
        if let Err(reply) = self.validate_agent_surface(&surface, keyboard_op) {
            return reply;
        }
        let keyboard_grab = self.agent.keyboard.is_grabbed();
        let matching_popup = self
            .agent
            .keyboard
            .with_grab(|_, grab| grab.is::<PopupKeyboardGrab<WaylandState>>())
            .unwrap_or(false)
            && self.delivery_target_on(SeatKind::Agent, true) == Some((id, generation));
        if keyboard_grab && !matching_popup {
            return Self::agent_refusal("keyboard_grab");
        }
        if self.agent.pointer.is_grabbed()
            && self.delivery_target_on(SeatKind::Agent, false) != Some((id, generation))
        {
            return Self::agent_refusal("pointer_grab");
        }
        {
            // Resolve every pointer check before mutating either device focus.
            let pointer_target = if keyboard_op {
                None
            } else {
                let record = &self.surfaces[&object];
                let position = self
                    .agent
                    .pointer_position
                    .filter(|_| {
                        self.delivery_target_on(SeatKind::Agent, false) == Some((id, generation))
                    })
                    .unwrap_or((
                        f64::from(record.layout.x + record.layout.width / 2.0),
                        f64::from(record.layout.y + record.layout.height / 2.0),
                    ));
                let Some((hit, origin)) = self.agent_hit(Some(&object), position.0, position.1)
                else {
                    return Self::agent_refusal("chrome_target");
                };
                if let Err(reply) = self.validate_agent_surface(&hit, false) {
                    return reply;
                }
                Some((hit, origin, position))
            };
            if !matching_popup {
                let keyboard = self.agent.keyboard.clone();
                keyboard.set_focus(
                    self,
                    Some(SeatFocusTarget::Wayland(surface)),
                    SERIAL_COUNTER.next_serial(),
                );
            }
            if let Some((surface, origin, position)) = pointer_target {
                self.agent_motion(
                    Some((SeatFocusTarget::Wayland(surface), origin)),
                    position,
                    monotonic_millis(),
                );
            }
        }
        self.service_input_payload(SeatKind::Agent, op, Some((id, generation)))
    }

    /// Hit-testing uses wl_surface layout origins; callers translate window-local
    /// coordinates from the window-geometry origin. Descendant layouts contain subsurface
    /// and popup offsets. Use committed input regions and compositor tree order;
    /// ignore visibility/workspace only when a particular root was requested.
    fn agent_hit(
        &self,
        root: Option<&ObjectId>,
        x: f64,
        y: f64,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        self.surfaces
            .values()
            .filter(|record| {
                self.agent_tree_mapped(record)
                    && self.surface_is_input_presentable(record)
                    && root.map_or(record.layout.visible, |root| {
                        canonical_root_surface(&self.popup_manager, record.role.wl_surface()).id()
                            == *root
                    })
                    && x >= f64::from(record.layout.x)
                    && y >= f64::from(record.layout.y)
                    && x < f64::from(record.layout.x + record.layout.width)
                    && y < f64::from(record.layout.y + record.layout.height)
                    && record.committed_input_region.as_ref().is_none_or(|region| {
                        region.contains((
                            (x - f64::from(record.layout.x)).floor() as i32,
                            (y - f64::from(record.layout.y)).floor() as i32,
                        ))
                    })
            })
            .max_by(|left, right| surface_stack_cmp(left, right))
            .map(|record| {
                (
                    record.role.wl_surface().clone(),
                    (f64::from(record.layout.x), f64::from(record.layout.y)).into(),
                )
            })
    }

    pub(super) fn agent_tree_mapped<'a>(&'a self, mut record: &'a SurfaceRecord) -> bool {
        loop {
            if !record.mapped || matches!(record.role, SurfaceRole::Dormant(_)) {
                return false;
            }
            if matches!(record.role, SurfaceRole::Subsurface { .. })
                && !record.parent_association_committed
            {
                return false;
            }
            let Some(parent) = record.layout.parent else {
                return true;
            };
            let Some(parent) = self
                .surface_objects
                .get(&parent)
                .and_then(|object| self.surfaces.get(object))
            else {
                return false;
            };
            record = parent;
        }
    }

    pub(super) fn reconcile_agent_focus(&mut self) {
        self.prune_agent_popup_grab();
        let invalid = |surface: &WlSurface| {
            self.surfaces
                .get(&surface.id())
                .is_none_or(|record| !self.agent_tree_mapped(record))
        };
        let keyboard_dead = self.agent.keyboard.current_focus()
            .and_then(|target| target.owned_surface()).as_ref().is_some_and(invalid);
        let pointer_dead = self.agent.pointer.current_focus()
            .and_then(|target| target.owned_surface()).as_ref().is_some_and(invalid);
        let parent = self.agent.keyboard_ancestors.iter().find(|surface| !invalid(surface)).cloned();
        if pointer_dead {
            self.agent.last_pointer_action = None;
            let pointer = self.agent.pointer.clone();
            // ClickGrab ignores motion's requested focus. Retire its buttons
            // first so the clearing motion runs after the implicit grab ends.
            self.release_agent_device_holds(false);
            pointer.motion(self, None, &MotionEvent {
                location: pointer.current_location(),
                serial: SERIAL_COUNTER.next_serial(), time: monotonic_millis(),
            });
            pointer.frame(self);
        }
        if keyboard_dead && self.agent_grabbed_keyboard_surface().is_none() {
            let keyboard = self.agent.keyboard.clone();
            keyboard.unset_grab(self);
            if parent.is_none() { self.release_agent_device_holds(true); }
            keyboard.set_focus(self, parent.map(SeatFocusTarget::Wayland), SERIAL_COUNTER.next_serial());
        }
    }

    fn agent_grabbed_keyboard_surface(&self) -> Option<WlSurface> {
        if !self.agent.keyboard.with_grab(|_, grab| grab.is::<PopupKeyboardGrab<WaylandState>>()).unwrap_or(false) {
            return None;
        }
        let grab = self.agent.popup_grab.as_ref().filter(|grab| !grab.has_ended())?;
        let surface = grab.current_grab()?.owned_surface()?;
        self.popup_manager.find_popup(&surface)?;
        Some(surface)
    }

    /// PopupKeyboardGrab refocuses on input. Validate that same destination
    /// before injection, even if its old submenu focus has already disappeared.
    pub(super) fn agent_keyboard_delivery_surface(&self) -> Option<WlSurface> {
        self.agent_grabbed_keyboard_surface().or_else(|| {
            self.agent.keyboard.current_focus().and_then(|target| target.owned_surface())
        })
    }

    fn prune_agent_popup_grab(&mut self) {
        if self.agent.popup_grab.as_ref().is_some_and(|grab| {
            grab.has_ended() || grab.current_grab().and_then(|focus| focus.owned_surface())
                .is_none_or(|surface| self.popup_manager.find_popup(&surface).is_none())
        }) {
            // Smithay can retain dismissed resources until client destruction;
            // current_grab already falls back to the root when no popup remains.
            // Retire the installed handles as well as our stored chain: an
            // ended PopupPointerGrab must not block a later targeted key.
            self.retire_agent_popup_handles();
        }
        let pointer = self.agent.pointer.clone();
        if pointer.current_pressed().is_empty()
            && pointer.with_grab(|_, grab| grab.is::<smithay::input::pointer::ClickGrab<WaylandState>>()).unwrap_or(false)
        {
            pointer.unset_grab_without_focus_restore(self, SERIAL_COUNTER.next_serial(), monotonic_millis());
        }
    }

    fn agent_motion(
        &mut self,
        focus: Option<(SeatFocusTarget, Point<f64, Logical>)>,
        position: (f64, f64),
        time: u32,
    ) {
        let next_root = focus.as_ref().and_then(|(target, _)| target.owned_surface())
            .map(|surface| canonical_root_surface(&self.popup_manager, &surface));
        if self.agent.last_pointer_action.as_ref().is_some_and(|(_, root)| Some(root) != next_root.as_ref()) {
            self.agent.last_pointer_action = None;
        }
        self.agent.pointer_position = Some(position);
        let pointer = self.agent.pointer.clone();
        pointer.motion(
            self,
            focus,
            &MotionEvent {
                location: position.into(),
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        pointer.frame(self);
    }

    pub(super) fn move_agent_pointer(
        &mut self,
        target: &PointerMoveTarget,
        time: u32,
    ) -> Result<(), ControlReply> {
        let (root, x, y) = match target {
            PointerMoveTarget::Window {
                id,
                generation,
                x,
                y,
                ..
            } => {
                let object = self
                    .resolve_window_target(*id, Some(*generation))
                    .map_err(|error| ControlReply::WindowTarget { id: *id, error })?;
                let record = &self.surfaces[&object];
                self.validate_agent_surface(record.role.wl_surface(), false)?;
                let position = (
                    f64::from(record.window_origin.0) + x,
                    f64::from(record.window_origin.1) + y,
                );
                (Some(object), position.0, position.1)
            }
            PointerMoveTarget::Relative { dx, dy } => {
                let (x, y) = self
                    .agent
                    .pointer_position
                    .ok_or_else(|| Self::agent_refusal("no_pointer_target"))?;
                (None, x + dx, y + dy)
            }
            PointerMoveTarget::Output { .. } => {
                let HostInput::PointerMotionAbsolute { x, y, .. } =
                    self.pointer_move_input(target, time)?
                else {
                    unreachable!()
                };
                (None, x, y)
            }
        };
        let click_grab = self.agent.pointer
            .with_grab(|_, grab| grab.is::<smithay::input::pointer::ClickGrab<WaylandState>>())
            .unwrap_or(false);
        if self.agent.pointer.is_grabbed() && !self.agent_popup_pointer_grab() {
            if !click_grab {
                return Err(Self::agent_refusal("pointer_grab"));
            }
            // The ordinary implicit button grab may continue within its root.
            let start = self
                .agent
                .pointer
                .grab_start_data()
                .and_then(|start| start.focus)
                .and_then(|(focus, _)| focus.owned_surface());
            if start.is_none_or(|surface| {
                root.as_ref().is_some_and(|root| {
                    canonical_root_surface(&self.popup_manager, &surface).id() != *root
                })
            }) {
                return Err(Self::agent_refusal("pointer_grab"));
            }
        }
        #[cfg(feature = "embedded-quoin")]
        if root.is_none() && !click_grab
            && self.embedded_shell.as_ref().is_some_and(|bridge| bridge.covers(x, y))
        {
            return Err(Self::agent_refusal("chrome_target"));
        }
        if root.is_none() && !click_grab
            && matches!(
                self.pointer_target_at(x, y),
                Some(PointerTarget::Chrome { .. })
            )
        {
            return Err(Self::agent_refusal("chrome_target"));
        }
        let hit = self.agent_hit(root.as_ref(), x, y);
        if root.is_some() && hit.is_none() {
            return Err(Self::agent_refusal("chrome_target"));
        }
        if let Some((surface, _)) = &hit {
            self.validate_agent_surface(surface, false)?;
        }
        self.agent_motion(
            hit.map(|(surface, origin)| (SeatFocusTarget::Wayland(surface), origin)),
            (x, y),
            time,
        );
        Ok(())
    }

    pub(super) fn deliver_agent_input(&mut self, input: HostInput) {
        match input {
            HostInput::Key {
                keycode,
                state,
                time,
            } => {
                let keyboard = self.agent.keyboard.clone();
                let serial = SERIAL_COUNTER.next_serial();
                keyboard.input::<(), _>(
                    self,
                    keycode,
                    smithay_key_state(state),
                    serial,
                    time,
                    |state, _, _| {
                        state.agent.delivery.key_handled =
                            state.agent.keyboard.current_focus().is_some();
                        state.agent.delivery.key_delivery =
                            state.delivery_target_on(SeatKind::Agent, true);
                        smithay::input::keyboard::FilterResult::Forward
                    },
                );
                if state == HostButtonState::Pressed {
                    self.agent.last_keyboard_action = keyboard
                        .current_focus()
                        .and_then(|target| target.owned_surface())
                        .map(|surface| {
                            (
                                serial,
                                canonical_root_surface(&self.popup_manager, &surface),
                            )
                        });
                }
            }
            HostInput::PointerButton {
                button,
                state,
                time,
            } => {
                self.agent.delivery.button_delivery =
                    self.delivery_target_on(SeatKind::Agent, false);
                let pointer = self.agent.pointer.clone();
                let serial = SERIAL_COUNTER.next_serial();
                pointer.button(
                    self,
                    &ButtonEvent {
                        serial,
                        time,
                        button,
                        state: smithay_button_state(state),
                    },
                );
                if state == HostButtonState::Pressed {
                    // A popup grab can dismiss itself and forward this press
                    // to pending focus. Record its actual delivery root.
                    self.agent.last_pointer_action = pointer.current_focus()
                        .and_then(|target| target.owned_surface())
                        .map(|surface| (serial, canonical_root_surface(&self.popup_manager, &surface)));
                }
                pointer.frame(self);
            }
            HostInput::PointerAxis {
                horizontal,
                vertical,
                source,
                relative_direction,
                time,
            } => {
                let mut frame = AxisFrame::new(time).source(source);
                let mut carries_anything = false;
                for (axis, direction, report) in [
                    (Axis::Horizontal, relative_direction.0, horizontal),
                    (Axis::Vertical, relative_direction.1, vertical),
                ] {
                    let Some(report) = report else { continue };
                    let stops = report.amount == 0.0
                        && matches!(source, AxisSource::Finger | AxisSource::Continuous);
                    if !stops && report.amount == 0.0 && report.v120.is_none_or(|value| value == 0)
                    {
                        continue;
                    }
                    carries_anything = true;
                    frame = frame
                        .relative_direction(axis, direction)
                        .value(axis, report.amount);
                    if let Some(value) = report.v120 {
                        frame = frame.v120(axis, value);
                    }
                    if stops {
                        frame = frame.stop(axis);
                    }
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
        self.prune_agent_popup_grab();
    }
}
