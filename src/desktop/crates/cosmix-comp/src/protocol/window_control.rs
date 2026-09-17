//! Window-addressed Bus control: `{id, generation}` target fencing, the
//! minimise/restore operations shared by `windows.s<id>.minimized` and the
//! `comp.window.*` verbs, and the event-driven `comp.window.wait` /
//! `close {force}` waiters.

use serde_json::{Value, json};
use smithay::reexports::calloop::{
    RegistrationToken,
    timer::{TimeoutAction, Timer},
};
use smithay::reexports::wayland_server::backend::DisconnectReason;

use super::*;
use crate::port::{ControlReply, PlaceSpec, WaitSpec, WaitUntil, WindowMatch, WindowOp};

/// Why a window-addressed request did not resolve to a window. These are
/// correctness refusals (the request is aimed at the wrong thing), never
/// caller checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WindowTargetError {
    /// No live, role-bearing surface has this id.
    UnknownWindow,
    /// The id is live but now names a different role assignment.
    StaleTarget { requested: u64, current: u64 },
    /// The surface exists but is not a managed toplevel (popup, layer,
    /// override-redirect X11, ...).
    NotManaged,
    /// A managed toplevel with no mapped content.
    NotMapped,
}

impl WaylandState {
    /// The one resolver for every window-addressed request. `generation`
    /// is optional so id-only callers keep working; when given it must
    /// match the surface's current role generation.
    pub(crate) fn resolve_window_target(
        &self,
        id: u64,
        generation: Option<u64>,
    ) -> Result<ObjectId, WindowTargetError> {
        let object = self
            .surface_objects
            .get(&SurfaceId(id))
            .ok_or(WindowTargetError::UnknownWindow)?;
        let record = self
            .surfaces
            .get(object)
            .ok_or(WindowTargetError::UnknownWindow)?;
        if let Some(requested) = generation
            && requested != record.generation
        {
            return Err(WindowTargetError::StaleTarget {
                requested,
                current: record.generation,
            });
        }
        if matches!(record.role, SurfaceRole::Dormant(_)) {
            return Err(WindowTargetError::UnknownWindow);
        }
        if !record.role.managed_toplevel() {
            return Err(WindowTargetError::NotManaged);
        }
        if !record.mapped {
            return Err(WindowTargetError::NotMapped);
        }
        Ok(object.clone())
    }

    /// Minimise (`true`) or restore (`false`) one resolved window. Returns
    /// the value before and after; a no-op leaves both equal.
    pub(crate) fn set_window_minimized(
        &mut self,
        object: &ObjectId,
        minimized: bool,
    ) -> Option<(bool, bool)> {
        let record = self.surfaces.get(object)?;
        let before = record.minimized;
        let surface = record.role.wl_surface().clone();
        if minimized && !before {
            self.minimize_toplevel(&surface);
        } else if !minimized && before {
            self.restore_window(object);
        }
        let after = self.surfaces.get(object)?.minimized;
        Some((before, after))
    }

    fn window_reply(&self, object: &ObjectId, id: u64, changed: bool) -> ControlReply {
        // The record can only vanish between the operation and this read if
        // the operation itself destroyed it; say so rather than `busy`.
        let Some(record) = self.surfaces.get(object) else {
            return ControlReply::WindowTarget {
                id,
                error: WindowTargetError::UnknownWindow,
            };
        };
        ControlReply::Window {
            id: record.id.0,
            generation: record.generation,
            title: record.title.clone(),
            app_id: record.app_id.clone(),
            minimized: record.minimized,
            changed,
        }
    }

    /// Mapped managed windows currently minimised. Every such window is on
    /// the minimise LIFO, so when `restore {}` finds the LIFO empty this is
    /// 0; it is still reported (as `not_found.minimized_count`) so a lost
    /// LIFO entry would show up to the caller instead of hiding.
    fn minimized_window_count(&self) -> usize {
        self.surfaces
            .values()
            .filter(|record| record.mapped && record.minimized && record.role.managed_toplevel())
            .count()
    }

    /// The entry the LIFO pop will restore: the newest one that is still a
    /// mapped, minimised, managed toplevel (the pop discards older invalid
    /// entries on its way down).
    fn next_lifo_restore(&self) -> Option<(ObjectId, SurfaceId)> {
        self.minimized_toplevels.iter().rev().find_map(|object| {
            let record = self.surfaces.get(object)?;
            (record.mapped && record.minimized && record.role.managed_toplevel())
                .then(|| (object.clone(), record.id))
        })
    }

    /// Every one-pass `comp.window.*` verb. A session lock refuses them all:
    /// the lock owns what is on screen until it ends.
    pub(crate) fn service_window_op(&mut self, op: &WindowOp) -> ControlReply {
        if self.session_lock_active() {
            return ControlReply::Locked;
        }
        let (code, id, generation) = match op {
            WindowOp::Minimize { id, generation } => (1, *id, *generation),
            WindowOp::Restore { target } => {
                let (id, generation) = target.unwrap_or_default();
                (2, id, generation)
            }
            WindowOp::Focus { id, generation, .. } => (3, *id, *generation),
            WindowOp::Raise { id, generation } => (4, *id, *generation),
            WindowOp::Close { id, generation } => (5, *id, *generation),
            WindowOp::Place(spec) => (6, spec.id, spec.generation),
        };
        crate::frame_trace::event("comp_window_control", || (id, code, generation));
        match op {
            WindowOp::Minimize { .. } | WindowOp::Restore { .. } => self.service_minimize_op(op),
            WindowOp::Focus {
                id,
                generation,
                raise,
            } => self.service_window_focus(*id, *generation, *raise),
            WindowOp::Raise { id, generation } => self.service_window_raise(*id, *generation),
            WindowOp::Close { id, generation } => {
                let object = match self.resolve_window_target(*id, Some(*generation)) {
                    Ok(object) => object,
                    Err(error) => return ControlReply::WindowTarget { id: *id, error },
                };
                let surface = self.surfaces[&object].role.wl_surface().clone();
                self.close_managed_toplevel(&surface);
                ControlReply::Body(json!({
                    "id": id,
                    "generation": generation,
                    "closed": "polite",
                }))
            }
            WindowOp::Place(spec) => self.service_window_place(spec),
        }
    }

    fn service_minimize_op(&mut self, op: &WindowOp) -> ControlReply {
        let (target, minimized) = match *op {
            WindowOp::Minimize { id, generation } => ((id, generation), true),
            WindowOp::Restore {
                target: Some(target),
            } => (target, false),
            WindowOp::Restore { target: None } => {
                // Mark first: the first cause recorded for a surface wins,
                // and the restore itself would record "wayland.focus".
                let expected = self.next_lifo_restore();
                if let Some((_, id)) = &expected {
                    self.mark_surface_dirty(*id, "comp.window");
                }
                let restored = self.restore_most_recently_minimized();
                debug_assert_eq!(
                    restored.as_ref(),
                    expected.as_ref().map(|(object, _)| object),
                    "the LIFO pop restores the predicted entry"
                );
                return match (restored, expected) {
                    (Some(object), Some((_, id))) => self.window_reply(&object, id.0, true),
                    (Some(object), None) => {
                        let id = self.surfaces.get(&object).map_or(0, |record| record.id.0);
                        self.window_reply(&object, id, true)
                    }
                    (None, _) => {
                        let minimized_count = self.minimized_window_count();
                        debug_assert_eq!(
                            minimized_count, 0,
                            "every minimised window is on the LIFO"
                        );
                        ControlReply::NotFound { minimized_count }
                    }
                };
            }
            WindowOp::Focus { .. }
            | WindowOp::Raise { .. }
            | WindowOp::Close { .. }
            | WindowOp::Place(_) => {
                unreachable!("only minimise and restore are routed here")
            }
        };
        let (id, generation) = target;
        let object = match self.resolve_window_target(id, Some(generation)) {
            Ok(object) => object,
            Err(error) => return ControlReply::WindowTarget { id, error },
        };
        self.mark_surface_dirty(SurfaceId(id), "comp.window");
        match self.set_window_minimized(&object, minimized) {
            Some((before, after)) => self.window_reply(&object, id, before != after),
            None => ControlReply::WindowTarget {
                id,
                error: WindowTargetError::UnknownWindow,
            },
        }
    }
}

/// Registered `comp.window.wait` and `comp.window.close {force}` waiters.
/// Each is checked once per dispatch cycle, after the cycle's state edges,
/// and holds one calloop timer for its deadline: nothing polls.
#[derive(Default)]
pub(crate) struct WindowWaiters {
    next: u64,
    pub(super) waiters: BTreeMap<u64, WindowWaiter>,
}

pub(super) struct WindowWaiter {
    kind: WaiterKind,
    started: Instant,
    timer: Option<RegistrationToken>,
    reply: tokio::sync::oneshot::Sender<ControlReply>,
}

enum WaiterKind {
    Wait(WaitSpec),
    ForceClose { id: u64, generation: u64 },
}

fn geometry_size(record: &SurfaceRecord) -> (i32, i32) {
    record.committed_window_geometry.map_or(
        (
            record.layout.width.round() as i32,
            record.layout.height.round() as i32,
        ),
        |geometry| {
            (
                geometry.width.round() as i32,
                geometry.height.round() as i32,
            )
        },
    )
}

fn names_match(filter: &WindowMatch, record: &SurfaceRecord) -> bool {
    filter
        .app_id
        .as_deref()
        .is_none_or(|app_id| record.app_id.as_deref() == Some(app_id))
        && filter
            .title
            .as_deref()
            .is_none_or(|title| record.title.as_deref() == Some(title))
        && filter.title_contains.as_deref().is_none_or(|needle| {
            record
                .title
                .as_deref()
                .is_some_and(|title| title.contains(needle))
        })
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

impl WaylandState {
    fn service_window_focus(&mut self, id: u64, generation: u64, raise: bool) -> ControlReply {
        let object = match self.resolve_window_target(id, Some(generation)) {
            Ok(object) => object,
            Err(error) => return ControlReply::WindowTarget { id, error },
        };
        let record = &self.surfaces[&object];
        let surface = record.role.wl_surface().clone();
        // The same candidacy Alt+Tab uses; a refusal says which gate held.
        let reason = if self.highest_exclusive_layer().is_some() {
            Some("exclusive_layer")
        } else if record.minimized {
            Some("minimized")
        } else if !record.layout.visible {
            Some("not_visible")
        } else if !self.surface_is_input_presentable(record) {
            Some("not_presentable")
        } else {
            None
        };
        if reason.is_none() {
            self.mark_surface_dirty(SurfaceId(id), "comp.window");
            if raise {
                self.activate_managed_window(&surface);
            } else {
                self.arbitrate_keyboard_focus(Some(surface), false, false);
            }
        }
        let focused = self
            .surfaces
            .get(&object)
            .is_some_and(|record| record.focused);
        let mut body = json!({"id": id, "generation": generation, "focused": focused});
        if let Some(reason) = reason.or((!focused).then_some("refused")) {
            body["reason"] = json!(reason);
        }
        ControlReply::Body(body)
    }

    fn service_window_raise(&mut self, id: u64, generation: u64) -> ControlReply {
        let object = match self.resolve_window_target(id, Some(generation)) {
            Ok(object) => object,
            Err(error) => return ControlReply::WindowTarget { id, error },
        };
        let record = &self.surfaces[&object];
        let before = record.layout.z;
        let surface = record.role.wl_surface().clone();
        self.mark_surface_dirty(SurfaceId(id), "comp.window");
        self.raise_surface(&surface);
        // Stacking decides what is under the cursor.
        self.retarget_pointer_after_visibility_change();
        let after = self.surfaces[&object].layout.z;
        ControlReply::Body(json!({
            "id": id,
            "generation": generation,
            "raised": after != before,
        }))
    }

    /// The output row a place addresses: the named one, else the window's
    /// own, else the default output.
    fn place_output(
        &self,
        id: u64,
        requested: Option<&str>,
    ) -> Result<(String, port_snapshot::OutputSnapshot), ControlReply> {
        let projection = port_snapshot::project_outputs(self).ok_or(ControlReply::Busy)?;
        if let Some(requested) = requested {
            return projection
                .rows
                .into_iter()
                .find(|(key, row)| key == requested || row.name == requested)
                .ok_or_else(|| {
                    ControlReply::refused("unknown_output", json!({"output": requested}))
                });
        }
        let current = port_snapshot::project_surface_by_id(self, SurfaceId(id), &projection.keys)
            .and_then(|row| row.output);
        let default = self
            .backend
            .default_output()
            .and_then(|output| port_snapshot::project_output(self, &output))
            .map(|(key, _)| key);
        current
            .into_iter()
            .chain(default)
            .find_map(|key| projection.rows.get(&key).cloned().map(|row| (key, row)))
            .ok_or_else(|| ControlReply::refused("unknown_output", json!({"output": null})))
    }

    fn service_window_place(&mut self, spec: &PlaceSpec) -> ControlReply {
        let object = match self.resolve_window_target(spec.id, Some(spec.generation)) {
            Ok(object) => object,
            Err(error) => {
                return ControlReply::WindowTarget { id: spec.id, error };
            }
        };
        let record = &self.surfaces[&object];
        let maximized = record.committed_maximized || record.requested_maximized;
        let fullscreen = record.committed_fullscreen || record.requested_fullscreen;
        if maximized || fullscreen {
            return ControlReply::refused(
                "invalid_state",
                json!({"id": spec.id, "maximized": maximized, "fullscreen": fullscreen}),
            );
        }
        let surface = record.role.wl_surface().clone();
        let origin = record.window_origin;
        let configured = record.configured_size;
        let (key, row) = match self.place_output(spec.id, spec.output.as_deref()) {
            Ok(output) => output,
            Err(reply) => return reply,
        };
        // An absent coordinate keeps the window's offset within its output
        // (the old one, when the place changes outputs).
        let (old_x, old_y) = match self.place_output(spec.id, None) {
            Ok((_, old)) => (old.x as f32, old.y as f32),
            Err(_) => (row.x as f32, row.y as f32),
        };
        let target = (
            row.x as f32 + spec.x.map_or(origin.0 - old_x, |x| x as f32),
            row.y as f32 + spec.y.map_or(origin.1 - old_y, |y| y as f32),
        );
        let requested = (spec.width.is_some() || spec.height.is_some()).then(|| {
            self.clamp_window_size(
                &surface,
                (
                    spec.width.unwrap_or(configured.0),
                    spec.height.unwrap_or(configured.1),
                ),
            )
        });
        // A client move/resize in progress would steer the window straight
        // back.
        if interactive_surface(self.interactive_pointer.as_ref())
            .is_some_and(|interactive| *interactive == surface)
        {
            self.finish_interactive_pointer(true);
        }
        let configure_pending = requested.is_some_and(|size| {
            self.resize_window_to(&surface, target, size, "comp.window")
                .unwrap_or(false)
        });
        if !configure_pending {
            self.move_window_to(&surface, target, "comp.window");
        }
        self.retarget_pointer_after_visibility_change();
        let placed = self
            .surfaces
            .get(&object)
            .map_or(target, |record| record.window_origin);
        ControlReply::Body(json!({
            "id": spec.id,
            "generation": spec.generation,
            "output": key,
            "window_x": placed.0 - row.x as f32,
            "window_y": placed.1 - row.y as f32,
            "requested": requested.map(|(width, height)| json!({"width": width, "height": height})),
            "configure_pending": configure_pending,
        }))
    }

    pub(crate) fn start_window_wait(
        &mut self,
        spec: WaitSpec,
        reply: tokio::sync::oneshot::Sender<ControlReply>,
    ) {
        let timeout = spec.timeout;
        crate::frame_trace::event("comp_window_control", || {
            (spec.window.id.unwrap_or(0), 7, millis(timeout))
        });
        self.register_waiter(WaiterKind::Wait(spec), timeout, reply);
    }

    /// `comp.window.close {force}`: the polite close now, the kill only if
    /// the same `{id, generation}` is still alive at the deadline.
    pub(crate) fn start_force_close(
        &mut self,
        id: u64,
        generation: u64,
        timeout: Duration,
        reply: tokio::sync::oneshot::Sender<ControlReply>,
    ) {
        crate::frame_trace::event("comp_window_control", || (id, 8, generation));
        if self.session_lock_active() {
            let _ = reply.send(ControlReply::Locked);
            return;
        }
        let object = match self.resolve_window_target(id, Some(generation)) {
            Ok(object) => object,
            Err(error) => {
                let _ = reply.send(ControlReply::WindowTarget { id, error });
                return;
            }
        };
        let surface = self.surfaces[&object].role.wl_surface().clone();
        self.close_managed_toplevel(&surface);
        self.register_waiter(WaiterKind::ForceClose { id, generation }, timeout, reply);
    }

    fn register_waiter(
        &mut self,
        kind: WaiterKind,
        timeout: Duration,
        reply: tokio::sync::oneshot::Sender<ControlReply>,
    ) {
        let key = self.window_waiters.next;
        self.window_waiters.next = key.wrapping_add(1);
        let timer = self.capture_loop_handle.insert_source(
            Timer::from_duration(timeout),
            move |_, _, state| {
                state.expire_window_waiter(key);
                TimeoutAction::Drop
            },
        );
        let timer = match timer {
            Ok(token) => token,
            Err(error) => {
                tracing::warn!(%error, "window wait timer unavailable");
                let _ = reply.send(ControlReply::Busy);
                return;
            }
        };
        self.window_waiters.waiters.insert(
            key,
            WindowWaiter {
                kind,
                started: Instant::now(),
                timer: Some(timer),
                reply,
            },
        );
        // A condition that already holds is answered now, not at the next
        // edge.
        self.service_window_waiters();
    }

    /// Resolve every waiter whose condition now holds. Runs at the end of
    /// each observation pass, after the cycle's state has settled.
    pub(crate) fn service_window_waiters(&mut self) {
        if self.window_waiters.waiters.is_empty() {
            return;
        }
        let keys = self
            .window_waiters
            .waiters
            .keys()
            .copied()
            .collect::<Vec<_>>();
        for key in keys {
            let Some(waiter) = self.window_waiters.waiters.get(&key) else {
                continue;
            };
            if waiter.reply.is_closed() {
                self.finish_window_waiter(key, None);
                continue;
            }
            let waited_ms = millis(waiter.started.elapsed());
            let outcome = match &waiter.kind {
                WaiterKind::Wait(spec) => self
                    .wait_outcome(spec)
                    .map(|window| wait_reply(spec, window, waited_ms)),
                WaiterKind::ForceClose { id, generation } => self
                    .resolve_window_target(*id, Some(*generation))
                    .is_err()
                    .then(|| close_reply(*id, *generation, "gone", waited_ms)),
            };
            if let Some(reply) = outcome {
                self.finish_window_waiter(key, Some(reply));
            }
        }
    }

    fn finish_window_waiter(&mut self, key: u64, reply: Option<ControlReply>) {
        let Some(waiter) = self.window_waiters.waiters.remove(&key) else {
            return;
        };
        if let Some(timer) = waiter.timer {
            self.capture_loop_handle.remove(timer);
        }
        if let Some(reply) = reply {
            let _ = waiter.reply.send(reply);
        }
    }

    /// The deadline timer fired (and drops itself).
    fn expire_window_waiter(&mut self, key: u64) {
        let Some(waiter) = self.window_waiters.waiters.remove(&key) else {
            return;
        };
        let waited_ms = millis(waiter.started.elapsed());
        let reply = match &waiter.kind {
            // An edge in this very dispatch may have beaten the timer.
            WaiterKind::Wait(spec) => match self.wait_outcome(spec) {
                Some(window) => wait_reply(spec, window, waited_ms),
                None => ControlReply::refused(
                    "timeout",
                    json!({"until": spec.until.name(), "waited_ms": waited_ms}),
                ),
            },
            WaiterKind::ForceClose { id, generation } => {
                self.force_close_deadline(*id, *generation, waited_ms)
            }
        };
        let _ = waiter.reply.send(reply);
    }

    fn force_close_deadline(&mut self, id: u64, generation: u64, waited_ms: u64) -> ControlReply {
        let Ok(object) = self.resolve_window_target(id, Some(generation)) else {
            return close_reply(id, generation, "gone", waited_ms);
        };
        let record = &self.surfaces[&object];
        if !matches!(record.role, SurfaceRole::Toplevel(_)) {
            // An X11 window's wl_surface belongs to Xwayland itself: a
            // client kill would take every X11 window down.
            return ControlReply::refused(
                "still_open",
                json!({"id": id, "generation": generation, "reason": "x11_kill_unsupported"}),
            );
        }
        let Some(client) = record.role.wl_surface().client() else {
            return close_reply(id, generation, "gone", waited_ms);
        };
        let pid = client
            .get_credentials(&self.display_handle)
            .ok()
            .map(|credentials| credentials.pid);
        let mut windows = self
            .surfaces
            .values()
            .filter(|record| {
                record.mapped
                    && record.role.managed_toplevel()
                    && record.role.wl_surface().client().as_ref() == Some(&client)
            })
            .map(|record| record.id.0)
            .collect::<Vec<_>>();
        windows.sort_unstable();
        tracing::info!(
            id,
            ?pid,
            ?windows,
            "comp.window.close force: killing the client"
        );
        self.display_handle
            .backend_handle()
            .kill_client(client.id(), DisconnectReason::ConnectionClosed);
        let ControlReply::Body(mut body) = close_reply(id, generation, "killed", waited_ms) else {
            unreachable!("close_reply builds a body");
        };
        body["scope"] = json!("client");
        body["pid"] = json!(pid);
        body["windows"] = json!(windows);
        ControlReply::Body(body)
    }

    /// The window row a wait resolves to (`null` for `unmapped` / `gone`),
    /// or `None` while the condition does not hold.
    fn wait_outcome(&self, spec: &WaitSpec) -> Option<Value> {
        let holds = |record: &SurfaceRecord| match spec.until {
            WaitUntil::Mapped => true,
            WaitUntil::Visible => record.layout.visible && !record.minimized,
            WaitUntil::Presented => self.presentation.ledger.counters(record.id).presented > 0,
            WaitUntil::Size { width, height } => geometry_size(record) == (width, height),
            WaitUntil::Focused => record.focused,
            WaitUntil::Unmapped | WaitUntil::Gone => false,
        };
        let live = |record: &SurfaceRecord| {
            record.mapped && record.role.managed_toplevel() && names_match(&spec.window, record)
        };
        if let Some(id) = spec.window.id {
            let record = self
                .surface_objects
                .get(&SurfaceId(id))
                .and_then(|object| self.surfaces.get(object))
                .filter(|record| {
                    !matches!(record.role, SurfaceRole::Dormant(_))
                        && spec
                            .window
                            .generation
                            .is_none_or(|generation| generation == record.generation)
                });
            return match spec.until {
                WaitUntil::Gone => record.is_none().then_some(Value::Null),
                WaitUntil::Unmapped => record
                    .is_none_or(|record| !record.mapped)
                    .then_some(Value::Null),
                _ => record
                    .filter(|record| live(record) && holds(record))
                    .map(|record| self.window_row(record.id)),
            };
        }
        match spec.until {
            WaitUntil::Gone | WaitUntil::Unmapped => self
                .surfaces
                .values()
                .all(|record| !live(record))
                .then_some(Value::Null),
            _ => self
                .surfaces
                .values()
                .filter(|record| live(record) && holds(record))
                .min_by_key(|record| record.id.0)
                .map(|record| self.window_row(record.id)),
        }
    }

    fn window_row(&self, id: SurfaceId) -> Value {
        port_snapshot::project_outputs(self)
            .and_then(|projection| port_snapshot::project_surface_by_id(self, id, &projection.keys))
            .and_then(|row| serde_json::to_value(port_snapshot::project_window_row(&row)).ok())
            .unwrap_or_else(|| json!({"id": id.0}))
    }
}

fn wait_reply(spec: &WaitSpec, window: Value, waited_ms: u64) -> ControlReply {
    ControlReply::Body(json!({
        "window": window,
        "until": spec.until.name(),
        "waited_ms": waited_ms,
    }))
}

fn close_reply(id: u64, generation: u64, closed: &str, waited_ms: u64) -> ControlReply {
    ControlReply::Body(json!({
        "id": id,
        "generation": generation,
        "closed": closed,
        "waited_ms": waited_ms,
    }))
}
