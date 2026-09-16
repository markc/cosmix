//! Text-input-v3 for layer-shell windows (the host has no Winit primary window).
use super::RunnerState;
use bevy::input_focus::InputFocus;
use bevy::prelude::*;
use bevy::text::EditableText;
use bevy::ui::{ComputedUiRenderTargetInfo, ComputedUiTargetCamera, UiGlobalTransform};
use bevy::window::{Ime, WindowEvent};
use wayland_client::protocol::{wl_seat::WlSeat, wl_surface::WlSurface};
use wayland_client::{Connection, Dispatch, QueueHandle, globals::GlobalList};
use wayland_protocols::wp::text_input::zv3::client::{
    zwp_text_input_manager_v3::ZwpTextInputManagerV3,
    zwp_text_input_v3::{self, ZwpTextInputV3},
};

pub(super) struct TextInputBridge {
    manager: Option<ZwpTextInputManagerV3>,
    input: Option<ZwpTextInputV3>,
    surface: Option<WlSurface>,
    enabled: Option<(Entity, Entity)>,
    preedit: Option<(String, Option<(usize, usize)>)>,
    committed: Option<String>,
    rectangle: Option<(i32, i32, i32, i32)>,
    generation: u64,
    batch_generation: Option<u64>,
    commit_serial: u32,
}

impl TextInputBridge {
    pub(super) fn new(globals: &GlobalList, qh: &QueueHandle<RunnerState>) -> Self {
        Self {
            manager: globals.bind(qh, 1..=1, ()).ok(),
            input: None,
            surface: None,
            enabled: None,
            preedit: None,
            committed: None,
            rectangle: None,
            generation: 0,
            batch_generation: None,
            commit_serial: 0,
        }
    }
    pub(super) fn attach(&mut self, seat: &WlSeat, qh: &QueueHandle<RunnerState>) {
        self.detach();
        self.input = self
            .manager
            .as_ref()
            .map(|manager| manager.get_text_input(seat, qh, ()));
    }
    pub(super) fn detach(&mut self) {
        if let Some(input) = self.input.take() {
            input.destroy();
        }
        self.surface = None;
        self.enabled = None;
        self.preedit = None;
        self.committed = None;
        self.rectangle = None;
        self.generation = self.generation.wrapping_add(1);
        self.batch_generation = None;
        self.commit_serial = 0;
    }
}

impl RunnerState {
    pub(super) fn sync_text_input(&mut self) {
        let Some(input) = self.text_input.input.as_ref() else {
            return;
        };
        let focused = self
            .app
            .world()
            .get_resource::<InputFocus>()
            .and_then(InputFocus::get);
        let target = self.text_input.surface.as_ref().and_then(|surface| {
            self.surface_targets()
                .into_iter()
                .find(|target| target.surface == *surface)
        });
        let enabled = focused.zip(target).and_then(|(entity, target)| {
            let world = self.app.world();
            world.get::<EditableText>(entity)?;
            let camera = world.get::<ComputedUiTargetCamera>(entity)?.get()?;
            let bevy::camera::RenderTarget::Window(bevy::window::WindowRef::Entity(window)) =
                world.get::<bevy::camera::RenderTarget>(camera)?
            else {
                return None;
            };
            (*window == target.window).then_some((entity, target.window))
        });
        if enabled != self.text_input.enabled {
            if self.text_input.enabled.is_some() {
                input.disable();
                input.commit();
                self.text_input.commit_serial = self.text_input.commit_serial.wrapping_add(1);
            }
            self.text_input.preedit = None;
            self.text_input.committed = None;
            self.text_input.rectangle = None;
            self.text_input.enabled = enabled;
            self.text_input.generation = self.text_input.generation.wrapping_add(1);
            self.text_input.batch_generation = None;
            if enabled.is_some() {
                input.enable();
                input.commit();
                self.text_input.commit_serial = self.text_input.commit_serial.wrapping_add(1);
            }
        }
        if let Some((entity, _)) = enabled {
            let world = self.app.world();
            let rect = world
                .get::<EditableText>(entity)
                .zip(world.get::<ComputedNode>(entity))
                .zip(world.get::<UiGlobalTransform>(entity))
                .zip(world.get::<ComputedUiRenderTargetInfo>(entity))
                .map(|(((editable, node), transform), target)| {
                    let cursor = editable.editor().ime_cursor_area();
                    let scroll = world
                        .get::<bevy::ui::widget::TextScroll>(entity)
                        .map_or(Vec2::ZERO, |s| s.0);
                    let local = Vec2::new(cursor.x0 as f32, cursor.y1 as f32)
                        + node.content_box().min
                        - scroll;
                    let point = transform.affine().transform_point2(local) / target.scale_factor();
                    (point.x as i32, point.y as i32, 1, 18)
                });
            if rect != self.text_input.rectangle
                && let Some((x, y, w, h)) = rect
            {
                input.set_cursor_rectangle(x, y, w, h);
                input.commit();
                self.text_input.commit_serial = self.text_input.commit_serial.wrapping_add(1);
                self.text_input.rectangle = rect;
            }
        }
    }

    fn emit_ime(&mut self, event: Ime) {
        self.app.world_mut().write_message(event.clone());
        self.app.world_mut().write_message(WindowEvent::Ime(event));
        self.needs_update = true;
    }
}

impl Dispatch<ZwpTextInputManagerV3, ()> for RunnerState {
    fn event(
        _: &mut Self,
        _: &ZwpTextInputManagerV3,
        _: <ZwpTextInputManagerV3 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpTextInputV3, ()> for RunnerState {
    fn event(
        state: &mut Self,
        input: &ZwpTextInputV3,
        event: zwp_text_input_v3::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if state.text_input.input.as_ref() != Some(input) {
            return;
        }
        let focus = state.app.world().get_resource::<InputFocus>().and_then(InputFocus::get);
        for event in state.text_input.receive(event, focus) {
            state.emit_ime(event);
        }
        state.needs_update = true;
    }
}

impl TextInputBridge {
    // This is the protocol dispatch path, shared by the real Wayland callback
    // and regression tests. The done serial identifies the client commit, not
    // whichever field happens to have focus when an old batch arrives.
    fn receive(&mut self, event: zwp_text_input_v3::Event, focus: Option<Entity>) -> Vec<Ime> {
        let mut events = Vec::new();
        match event {
            zwp_text_input_v3::Event::Enter { surface } => {
                self.surface = Some(surface);
            }
            zwp_text_input_v3::Event::Leave { .. } => {
                if let Some((_, window)) = self.enabled.take() {
                    events.push(Ime::Disabled { window });
                }
                self.surface = None;
                self.preedit = None;
                self.committed = None;
                self.generation = self.generation.wrapping_add(1);
                self.batch_generation = None;
            }
            zwp_text_input_v3::Event::PreeditString {
                text,
                cursor_begin,
                cursor_end,
            } => {
                let text = text.unwrap_or_default();
                let cursor = (cursor_begin >= 0
                    && cursor_end >= 0
                    && cursor_begin as usize <= text.len()
                    && cursor_end as usize <= text.len())
                .then_some((cursor_begin as usize, cursor_end as usize));
                self.batch_generation.get_or_insert(self.generation);
                self.preedit = Some((text, cursor));
            }
            zwp_text_input_v3::Event::CommitString { text } => {
                self.batch_generation.get_or_insert(self.generation);
                self.committed = text;
            }
            zwp_text_input_v3::Event::Done { serial } => {
                let committed = self.committed.take();
                let preedit = self.preedit.take();
                let generation = self.batch_generation.take();
                if let Some((entity, window)) = self.enabled
                    && focus == Some(entity)
                    && generation == Some(self.generation)
                    && serial == self.commit_serial
                {
                    if let Some(value) = committed {
                        events.push(Ime::Commit { window, value });
                    }
                    if let Some((value, cursor)) = preedit {
                        events.push(Ime::Preedit {
                            window,
                            value,
                            cursor,
                        });
                    }
                }
            }
            _ => {}
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn protocol_bridge_drops_delayed_batches_after_focus_change() {
        let mut world = World::new();
        let first = world.spawn_empty().id();
        let second = world.spawn_empty().id();
        let window = world.spawn_empty().id();
        let mut bridge = TextInputBridge {
            manager: None, input: None, surface: None,
            enabled: Some((first, window)), preedit: None, committed: None,
            rectangle: None, generation: 1, batch_generation: None, commit_serial: 1,
        };
        bridge.receive(zwp_text_input_v3::Event::CommitString { text: Some("old".into()) }, Some(first));
        bridge.enabled = Some((second, window));
        bridge.generation = 2;
        bridge.commit_serial = 3; // disable + enable commits
        assert!(bridge.receive(zwp_text_input_v3::Event::Done { serial: 1 }, Some(second)).is_empty());
        // An entire old batch can arrive after the new field was enabled.
        bridge.receive(zwp_text_input_v3::Event::PreeditString { text: Some("stale".into()), cursor_begin: 0, cursor_end: 0 }, Some(second));
        assert!(bridge.receive(zwp_text_input_v3::Event::Done { serial: 1 }, Some(second)).is_empty());
        bridge.receive(zwp_text_input_v3::Event::CommitString { text: Some("new".into()) }, Some(second));
        let events = bridge.receive(zwp_text_input_v3::Event::Done { serial: 3 }, Some(second));
        assert!(matches!(&events[..], [Ime::Commit { value, .. }] if value == "new"));
        assert!(bridge.receive(zwp_text_input_v3::Event::Done { serial: 3 }, Some(second)).is_empty());
    }
}
