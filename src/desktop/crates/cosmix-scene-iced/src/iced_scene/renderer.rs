//! `SurfaceRenderer` over `cosmix_iced_host::Surface`.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use cosmix_iced_host::core::keyboard::{self, key};
use cosmix_iced_host::core::mouse::{self, Interaction, ScrollDelta};
use cosmix_iced_host::core::widget::operation::Focusable;
use cosmix_iced_host::core::widget::{Id, Operation};
use cosmix_iced_host::core::{Event, Point, Rectangle, Size, SmolStr, input_method, window};
use cosmix_iced_host::{PixelFormat, Redraw, Settings, Surface};
use cosmix_scene::ResolvedScene;

use super::program::{Look, Outbox, SceneProgram};
use super::submit::FocusProbe;
use crate::surface::{
    CursorIcon, ImeEvent, ImeRequest, Key, Modifiers, NamedKey, PointerButton, Processed, Rect,
    ScrollUnit, SurfaceEvent, SurfaceRenderer,
};

/// The look every iced scene uses, replaced when the CTK design or
/// typography changes (`revision` increases).
#[derive(Clone, Debug)]
pub struct DesignShare {
    pub revision: u64,
    pub look: Look,
}

pub type SharedDesign = Arc<RwLock<DesignShare>>;

pub struct IcedSceneRenderer {
    surface: Surface<SceneProgram>,
    design: SharedDesign,
    design_revision: u64,
    scale: f32,
    modifiers: keyboard::Modifiers,
    composing: bool,
    scene_changed: bool,
}

impl IcedSceneRenderer {
    pub fn new(design: SharedDesign, outbox: Outbox) -> Self {
        let share = design.read().unwrap().clone();
        let surface = Surface::new(
            SceneProgram::new(share.look, outbox),
            Settings {
                default_font: share.look.font,
                default_text_size: share.look.text_px.into(),
                theme: cosmix_iced_host::Theme::Dark,
                background: Some(share.look.tokens.surface),
                ..Settings::default()
            },
        );
        Self {
            surface,
            design,
            design_revision: share.revision,
            scale: 1.0,
            modifiers: keyboard::Modifiers::empty(),
            composing: false,
            scene_changed: true,
        }
    }

    pub fn program(&self) -> &SceneProgram {
        self.surface.program()
    }

    /// Scene field ids whose widget has keyboard focus.
    pub fn focused(&mut self) -> HashSet<String> {
        let found = Arc::new(Mutex::new(HashSet::new()));
        self.surface.operate(Box::new(Collect(found.clone())));
        let found = std::mem::take(&mut *found.lock().unwrap());
        self.surface
            .program()
            .tree()
            .nodes
            .iter()
            .filter(|(id, node)| node.family == "field" && found.contains(&Id::from((*id).clone())))
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Logical bounds of scene node `key` as last laid out.
    pub fn node_bounds(&mut self, key: &str) -> Option<Rectangle> {
        let found = Arc::new(Mutex::new(None));
        self.surface.operate(Box::new(Bounds {
            id: super::program::node_id(key),
            found: found.clone(),
        }));
        found.lock().unwrap().take()
    }

    /// The host's last reported state (cursor, IME request with preedit, redraw).
    pub fn host_requests(&self) -> &cosmix_iced_host::Requests {
        self.surface.requests()
    }

    fn sync_design(&mut self) {
        let share = self.design.read().unwrap();
        if share.revision == self.design_revision {
            return;
        }
        self.design_revision = share.revision;
        let look = share.look;
        drop(share);
        self.surface.program_mut().set_look(look);
        self.surface.set_background(Some(look.tokens.surface));
        self.surface.invalidate();
        self.scene_changed = true;
    }

    fn logical(&self, x: f32, y: f32) -> Point {
        Point::new(x / self.scale, y / self.scale)
    }
}

struct Bounds {
    id: Id,
    found: Arc<Mutex<Option<Rectangle>>>,
}

impl Operation for Bounds {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }

    fn container(&mut self, id: Option<&Id>, bounds: Rectangle) {
        if id == Some(&self.id) {
            *self.found.lock().unwrap() = Some(bounds);
        }
    }
}

struct Collect(Arc<Mutex<HashSet<Id>>>);

impl Operation for Collect {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }

    fn focusable(&mut self, id: Option<&Id>, bounds: Rectangle, state: &mut dyn Focusable) {
        let mut probe = FocusProbe::default();
        probe.focusable(id, bounds, state);
        self.0.lock().unwrap().extend(probe.0);
    }
}

impl SurfaceRenderer for IcedSceneRenderer {
    fn resize(&mut self, width: u32, height: u32, scale: f32) {
        self.scale = scale;
        self.surface.resize(Size::new(width, height), scale);
        self.surface.invalidate();
    }

    fn set_scene(&mut self, scene: &ResolvedScene) {
        let focused = self.focused();
        self.surface.program_mut().set_scene(scene, &focused);
        self.scene_changed = true;
    }

    fn queue(&mut self, event: SurfaceEvent) {
        match event {
            SurfaceEvent::PointerMoved { x, y } => {
                let at = self.logical(x, y);
                self.surface.cursor_moved(at);
            }
            SurfaceEvent::PointerLeft => self.surface.cursor_left(),
            SurfaceEvent::PointerButton { button, pressed } => {
                let button = match button {
                    PointerButton::Primary => mouse::Button::Left,
                    PointerButton::Secondary => mouse::Button::Right,
                    PointerButton::Middle => mouse::Button::Middle,
                };
                self.surface.queue_event(Event::Mouse(if pressed {
                    mouse::Event::ButtonPressed(button)
                } else {
                    mouse::Event::ButtonReleased(button)
                }));
            }
            SurfaceEvent::Scroll { unit, x, y } => {
                let delta = match unit {
                    ScrollUnit::Line => ScrollDelta::Lines { x, y },
                    ScrollUnit::Pixel => ScrollDelta::Pixels {
                        x: x / self.scale,
                        y: y / self.scale,
                    },
                };
                self.surface
                    .queue_event(Event::Mouse(mouse::Event::WheelScrolled { delta }));
            }
            SurfaceEvent::Key {
                key: surface_key,
                latin,
                text,
                pressed,
                repeat,
                modifiers,
            } => {
                let modifiers = iced_modifiers(modifiers);
                if modifiers != self.modifiers {
                    self.modifiers = modifiers;
                    self.surface.queue_event(Event::Keyboard(
                        keyboard::Event::ModifiersChanged(modifiers),
                    ));
                }
                let key = iced_key(&surface_key);
                let physical_key = latin
                    .and_then(latin_code)
                    .map_or(key::Physical::Unidentified(key::NativeCode::Unidentified), key::Physical::Code);
                let location = keyboard::Location::Standard;
                let event = if pressed {
                    keyboard::Event::KeyPressed {
                        key: key.clone(),
                        modified_key: key,
                        physical_key,
                        location,
                        modifiers,
                        text: text.filter(|t| !t.is_empty()).map(SmolStr::new),
                        repeat,
                    }
                } else {
                    keyboard::Event::KeyReleased {
                        key: key.clone(),
                        modified_key: key,
                        physical_key,
                        location,
                        modifiers,
                    }
                };
                self.surface.queue_event(Event::Keyboard(event));
            }
            SurfaceEvent::Modifiers(modifiers) => {
                let modifiers = iced_modifiers(modifiers);
                if modifiers != self.modifiers {
                    self.modifiers = modifiers;
                    self.surface.queue_event(Event::Keyboard(
                        keyboard::Event::ModifiersChanged(modifiers),
                    ));
                }
            }
            SurfaceEvent::Focus(focused) => {
                self.surface.queue_event(Event::Window(if focused {
                    window::Event::Focused
                } else {
                    window::Event::Unfocused
                }));
            }
            SurfaceEvent::Ime(ime) => match ime {
                ImeEvent::Preedit { text, cursor } => {
                    if !self.composing {
                        self.composing = true;
                        self.surface
                            .queue_event(Event::InputMethod(input_method::Event::Opened));
                    }
                    self.surface.queue_event(Event::InputMethod(
                        input_method::Event::Preedit(text, cursor.map(|(a, b)| a..b)),
                    ));
                }
                ImeEvent::Commit(text) => {
                    self.surface
                        .queue_event(Event::InputMethod(input_method::Event::Commit(text)));
                }
                ImeEvent::Disabled => {
                    if self.composing {
                        self.composing = false;
                        self.surface
                            .queue_event(Event::InputMethod(input_method::Event::Closed));
                    }
                }
            },
        }
    }

    fn process(&mut self, now: Duration) -> Processed {
        self.sync_design();
        let update = self.surface.process();
        let clock = Instant::now();
        let deadline_due = matches!(update.redraw, Redraw::At(at) if at <= clock);
        let needs_redraw = update.needs_redraw || self.scene_changed || deadline_due;
        let wake_at = if needs_redraw {
            // The deadline after this draw is only known once it has run.
            Some(now)
        } else {
            match update.redraw {
                Redraw::Wait => None,
                Redraw::NextFrame => Some(now),
                Redraw::At(at) => Some(now + at.saturating_duration_since(clock)),
            }
        };
        let requests = self.surface.requests();
        let ime = match &requests.ime {
            cosmix_iced_host::ImeRequest::Disabled => ImeRequest::Disabled,
            cosmix_iced_host::ImeRequest::Enabled {
                physical_cursor, ..
            } => ImeRequest::Enabled {
                cursor: Rect::new(
                    physical_cursor.x,
                    physical_cursor.y,
                    physical_cursor.width,
                    physical_cursor.height,
                ),
            },
        };
        Processed {
            needs_redraw,
            cursor: cursor_icon(update.interaction),
            ime,
            wake_at,
        }
    }

    fn draw(&mut self, buffer: &mut [u8], width: u32, height: u32, stride: u32) -> Vec<Rect> {
        self.scene_changed = false;
        match self
            .surface
            .draw(buffer, width, height, stride, PixelFormat::Rgba8)
        {
            Ok(frame) => frame
                .damage
                .iter()
                .map(|d| Rect::new(d.x, d.y, d.width, d.height))
                .collect(),
            Err(error) => {
                bevy::log::warn!("iced scene draw refused: {error}");
                Vec::new()
            }
        }
    }
}

fn iced_modifiers(modifiers: Modifiers) -> keyboard::Modifiers {
    let mut out = keyboard::Modifiers::empty();
    out.set(keyboard::Modifiers::SHIFT, modifiers.shift);
    out.set(keyboard::Modifiers::CTRL, modifiers.control);
    out.set(keyboard::Modifiers::ALT, modifiers.alt);
    out.set(keyboard::Modifiers::LOGO, modifiers.logo);
    out
}

fn iced_key(surface_key: &Key) -> keyboard::Key {
    use key::Named as N;
    match surface_key {
        Key::Character(text) => keyboard::Key::Character(SmolStr::new(text)),
        Key::Unidentified => keyboard::Key::Unidentified,
        Key::Named(named) => keyboard::Key::Named(match named {
            NamedKey::Enter => N::Enter,
            NamedKey::Tab => N::Tab,
            NamedKey::Space => N::Space,
            NamedKey::Backspace => N::Backspace,
            NamedKey::Delete => N::Delete,
            NamedKey::Escape => N::Escape,
            NamedKey::ArrowLeft => N::ArrowLeft,
            NamedKey::ArrowRight => N::ArrowRight,
            NamedKey::ArrowUp => N::ArrowUp,
            NamedKey::ArrowDown => N::ArrowDown,
            NamedKey::Home => N::Home,
            NamedKey::End => N::End,
            NamedKey::PageUp => N::PageUp,
            NamedKey::PageDown => N::PageDown,
            NamedKey::Shift => N::Shift,
            NamedKey::Control => N::Control,
            NamedKey::Alt => N::Alt,
            NamedKey::Super => N::Super,
        }),
    }
}

/// The physical key for a layout-independent Latin letter or digit, so iced's
/// `Key::to_latin` resolves shortcuts on non-Latin layouts.
fn latin_code(latin: char) -> Option<key::Code> {
    use key::Code as C;
    const LETTERS: [C; 26] = [
        C::KeyA, C::KeyB, C::KeyC, C::KeyD, C::KeyE, C::KeyF, C::KeyG, C::KeyH, C::KeyI,
        C::KeyJ, C::KeyK, C::KeyL, C::KeyM, C::KeyN, C::KeyO, C::KeyP, C::KeyQ, C::KeyR,
        C::KeyS, C::KeyT, C::KeyU, C::KeyV, C::KeyW, C::KeyX, C::KeyY, C::KeyZ,
    ];
    const DIGITS: [C; 10] = [
        C::Digit0, C::Digit1, C::Digit2, C::Digit3, C::Digit4, C::Digit5, C::Digit6, C::Digit7,
        C::Digit8, C::Digit9,
    ];
    match latin.to_ascii_lowercase() {
        c @ 'a'..='z' => Some(LETTERS[(c as u8 - b'a') as usize]),
        c @ '0'..='9' => Some(DIGITS[(c as u8 - b'0') as usize]),
        _ => None,
    }
}

fn cursor_icon(interaction: Interaction) -> CursorIcon {
    match interaction {
        Interaction::Pointer => CursorIcon::Pointer,
        Interaction::Text => CursorIcon::Text,
        Interaction::Grab => CursorIcon::Grab,
        Interaction::Grabbing => CursorIcon::Grabbing,
        Interaction::NotAllowed | Interaction::NoDrop => CursorIcon::NotAllowed,
        Interaction::ResizingHorizontally | Interaction::ResizingColumn => {
            CursorIcon::ResizeHorizontal
        }
        Interaction::ResizingVertically | Interaction::ResizingRow => CursorIcon::ResizeVertical,
        _ => CursorIcon::Default,
    }
}
