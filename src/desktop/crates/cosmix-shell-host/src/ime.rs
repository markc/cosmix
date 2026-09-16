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
            }
            self.text_input.preedit = None;
            self.text_input.committed = None;
            self.text_input.rectangle = None;
            self.text_input.enabled = enabled;
            if enabled.is_some() {
                input.enable();
                input.commit();
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
        match event {
            zwp_text_input_v3::Event::Enter { surface } => {
                state.text_input.surface = Some(surface);
                state.needs_update = true;
            }
            zwp_text_input_v3::Event::Leave { .. } => {
                if let Some((_, window)) = state.text_input.enabled.take() {
                    state.emit_ime(Ime::Disabled { window });
                }
                state.text_input.surface = None;
                state.text_input.preedit = None;
                state.text_input.committed = None;
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
                state.text_input.preedit = Some((text, cursor));
            }
            zwp_text_input_v3::Event::CommitString { text } => state.text_input.committed = text,
            zwp_text_input_v3::Event::Done { .. } => {
                let committed = state.text_input.committed.take();
                let preedit = state.text_input.preedit.take();
                if let Some((entity, window)) = state.text_input.enabled
                    && state.app.world().resource::<InputFocus>().get() == Some(entity)
                {
                    if let Some(value) = committed {
                        state.emit_ime(Ime::Commit { window, value });
                    }
                    if let Some((value, cursor)) = preedit {
                        state.emit_ime(Ime::Preedit {
                            window,
                            value,
                            cursor,
                        });
                    }
                }
            }
            _ => {}
        }
    }
}
