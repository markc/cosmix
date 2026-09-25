//! Clipboard tasks and pointer gestures; selection itself lives in term-core.

use crate::{Action, Message, State, input, layout};
use cosmix_term_core::terminal::{MouseModifiers, SelectionSide, SelectionType, Terminal};
use iced::{
    Point, Task,
    mouse::{Button, Event},
};
use std::{
    cell::Cell,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Hit {
    pane: u64,
    col: u16,
    row: u16,
    side: SelectionSide,
}

#[derive(Clone)]
struct Press {
    pane: u64,
    terminal: Arc<Mutex<Terminal>>,
    last: Hit,
    button: u8,
    local: bool,
    origin: Point,
    mods: MouseModifiers,
}

#[derive(Default)]
pub(super) struct MouseState {
    pressed: Option<Press>,
    clicks: Option<(Hit, Instant, u8)>,
}

impl MouseState {
    /// End a cancelled grab exactly once, even after its pane has left the
    /// tab tree. Keep the original terminal alive until the release is sent.
    fn cancel(&mut self) -> bool {
        self.clicks = None;
        let Some(press) = self.pressed.take() else {
            return false;
        };
        let terminal = press.terminal.lock().expect("terminal");
        if !press.local {
            terminal.mouse_button(
                press.last.col,
                press.last.row,
                press.button,
                false,
                press.mods,
            );
        } else if press.button == 0 {
            terminal.selection_clear();
        }
        true
    }

    fn click(&mut self, hit: Hit, at: Instant) -> SelectionType {
        let count = self.clicks.map_or(1, |(last, when, count)| {
            if last.pane == hit.pane
                && last.col == hit.col
                && last.row == hit.row
                && at.saturating_duration_since(when) <= Duration::from_millis(300)
            {
                count % 3 + 1
            } else {
                1
            }
        });
        self.clicks = Some((hit, at, count));
        match count {
            2 => SelectionType::Semantic,
            3 => SelectionType::Lines,
            _ => SelectionType::Simple,
        }
    }
}

/// Widget-local state also sees presses/releases that iced has not delivered
/// to State yet. Positions travel WITH each event, never via the coalesced
/// hover position (which could already be the next event's position).
pub(super) struct MouseEvents {
    grabbed: Cell<Option<Button>>,
    last: Cell<Option<Hit>>,
}

impl MouseEvents {
    pub fn new(state: &State) -> Self {
        Self {
            grabbed: Cell::new(
                state
                    .mouse
                    .pressed
                    .as_ref()
                    .map(|press| match press.button {
                        0 => Button::Left,
                        1 => Button::Middle,
                        _ => Button::Right,
                    }),
            ),
            last: Cell::new(state.pointer.get().and_then(|p| state.hit(p, None))),
        }
    }

    pub fn message(
        &self,
        state: &State,
        event: &Event,
        position: Option<Point>,
    ) -> Option<Message> {
        let position = match event {
            Event::CursorMoved { position } => *position,
            _ => position.or(state.pointer.get())?,
        };
        let hit = state.hit(position, None);
        match event {
            Event::ButtonPressed(button) if button_code(*button).is_some() => {
                if hit.is_none() && self.grabbed.get().is_none() {
                    return None;
                }
                // A new button press replaces a cancelled Wayland grab,
                // including when the new press uses a different button.
                self.grabbed.set(hit.map(|_| *button));
            }
            Event::ButtonReleased(button) if button_code(*button).is_some() => {
                if self.grabbed.get() != Some(*button) {
                    return None;
                }
                self.grabbed.set(None);
            }
            Event::ButtonPressed(_) if self.grabbed.get().is_some() => {
                self.grabbed.set(None);
            }
            Event::CursorMoved { .. } => {
                let changed = self.last.replace(hit) != hit;
                if (!changed || hit.is_none()) && self.grabbed.get().is_none() {
                    return None;
                }
                if !changed && hit.is_some() {
                    return None;
                }
            }
            _ => return None,
        }
        Some(Message::Mouse(*event, position, Instant::now()))
    }
}

fn button_code(button: Button) -> Option<u8> {
    match button {
        Button::Left => Some(0),
        Button::Middle => Some(1),
        Button::Right => Some(2),
        _ => None,
    }
}

fn read(pane: u64, primary: bool) -> Task<Message> {
    let task = if primary {
        iced::clipboard::read_primary()
    } else {
        iced::clipboard::read()
    };
    task.map(move |text| Message::Paste(pane, text))
}

impl State {
    pub(super) fn cancel_mouse_gesture(&mut self) {
        self.paint_requested |= self.mouse.cancel();
    }

    pub(super) fn cancel_hidden_gesture(&mut self) {
        let hidden = self.mouse.pressed.as_ref().is_some_and(|press| {
            !self
                .tabs
                .lock()
                .expect("tabs")
                .leaves()
                .iter()
                .any(|pane| pane.id == press.pane)
        });
        if hidden {
            self.cancel_mouse_gesture();
        }
    }

    /// Clamp an active drag to its original pane even outside its rectangle.
    fn hit(&self, position: Point, target: Option<u64>) -> Option<Hit> {
        let tree = self.shape.tree.as_ref()?;
        let scale = self.painter.scale();
        let bounds = layout::content(self.window.width, self.window.height, scale);
        let (pane, rect) = layout::panes(tree, bounds, scale)
            .into_iter()
            .find(|(id, rect)| {
                target.map_or_else(
                    || {
                        position.x >= rect.x
                            && position.x < rect.x + rect.w
                            && position.y >= rect.y
                            && position.y < rect.y + rect.h
                    },
                    |target| *id == target,
                )
            })?;
        let cell = self.painter.logical_cell();
        let relative = Point::new(position.x - rect.x, position.y - rect.y);
        let border = layout::border(scale);
        let (col, row) = input::pointer_cell(relative, border, cell, *self.grids.get(&pane)?);
        let side = if relative.x - border - f32::from(col) * cell.0 < cell.0 / 2.0 {
            SelectionSide::Left
        } else {
            SelectionSide::Right
        };
        Some(Hit {
            pane,
            col,
            row,
            side,
        })
    }

    pub(super) fn clipboard_action(&mut self, action: Action) -> Task<Message> {
        let tabs = self.tabs.lock().expect("tabs");
        if tabs.is_empty() {
            return Task::none();
        }
        tabs.user_activity();
        let pane = tabs.active_tab().active_pane;
        if action == Action::Paste {
            return read(pane, false);
        }
        let text = tabs
            .active_terminal()
            .lock()
            .expect("terminal")
            .selection_text();
        text.map_or_else(Task::none, iced::clipboard::write)
    }

    pub(super) fn paste(&mut self, pane: u64, text: Option<String>) {
        let Some(text) = text else { return };
        // IDs never recycle. A closed pane drops a late answer; changing focus
        // or tabs cannot redirect it to a different shell.
        let tabs = self.tabs.lock().expect("tabs");
        let Some(terminal) = tabs.pane_by_id(pane) else {
            return;
        };
        tabs.user_activity();
        drop(tabs);
        match terminal.lock().expect("terminal").paste(&text) {
            Ok(()) => self.paste_notice = None,
            Err(error) => {
                eprintln!("term: paste: {error}");
                self.paste_notice = Some(error);
            }
        }
    }

    pub(super) fn mouse_event(
        &mut self,
        event: Event,
        position: Point,
        at: Instant,
    ) -> Task<Message> {
        let mods = MouseModifiers {
            shift: self.modifiers.shift(),
            alt: self.modifiers.alt(),
            ctrl: self.modifiers.control(),
        };
        match event {
            Event::ButtonPressed(button) => {
                // A compositor may cancel the grab without a button-up.
                if self.mouse.pressed.is_some() {
                    self.cancel_mouse_gesture();
                }
                let Some(button) = button_code(button) else {
                    return Task::none();
                };
                let Some(hit) = self.hit(position, None) else {
                    return Task::none();
                };
                let mut tabs = self.tabs.lock().expect("tabs");
                let Some(terminal) = tabs.pane_by_id(hit.pane) else {
                    return Task::none();
                };
                tabs.user_activity();
                tabs.focus(hit.pane);
                let local = mods.shift || !terminal.lock().expect("terminal").mouse_reporting();
                if local && button == 0 {
                    for pane in tabs.control_panes() {
                        if pane.id != hit.pane
                            && let Some(other) = tabs.pane_by_id(pane.id)
                        {
                            other.lock().expect("terminal").selection_clear();
                        }
                    }
                }
                drop(tabs);
                self.mouse.pressed = Some(Press {
                    pane: hit.pane,
                    terminal: terminal.clone(),
                    last: hit,
                    button,
                    local,
                    origin: position,
                    mods,
                });
                let terminal = terminal.lock().expect("terminal");
                if !local {
                    self.mouse.clicks = None;
                    terminal.mouse_button(hit.col, hit.row, button, true, mods);
                } else if button == 0 {
                    let kind = self.mouse.click(hit, at);
                    terminal.selection_start(hit.col, hit.row, hit.side, kind);
                    self.paint_requested = true;
                } else if button == 1 {
                    self.mouse.clicks = None;
                    return read(hit.pane, true);
                }
            }
            Event::CursorMoved { .. } | Event::ButtonReleased(_) => {
                let press = self.mouse.pressed.clone();
                let released = matches!(event, Event::ButtonReleased(_));
                if let Event::ButtonReleased(button) = event
                    && press
                        .as_ref()
                        .is_none_or(|press| Some(press.button) != button_code(button))
                {
                    return Task::none();
                }
                let Some(hit) = self.hit(position, press.as_ref().map(|p| p.pane)) else {
                    self.cancel_mouse_gesture();
                    return Task::none();
                };
                let tabs = self.tabs.lock().expect("tabs");
                let Some(terminal) = tabs.pane_by_id(hit.pane) else {
                    drop(tabs);
                    self.cancel_mouse_gesture();
                    return Task::none();
                };
                drop(tabs);
                if released {
                    self.mouse.pressed = None;
                } else if let Some(press) = self.mouse.pressed.as_mut() {
                    press.last = hit;
                }
                let terminal = terminal.lock().expect("terminal");
                if let Some(press) = press {
                    if press.local && press.button == 0 {
                        terminal.selection_update(hit.col, hit.row, hit.side);
                        self.paint_requested = true;
                        if (position.x - press.origin.x).abs() > 5.0
                            || (position.y - press.origin.y).abs() > 5.0
                        {
                            self.mouse.clicks = None;
                        }
                        if released {
                            return terminal
                                .selection_finish()
                                .map_or_else(Task::none, iced::clipboard::write_primary);
                        }
                    } else if !press.local {
                        if released {
                            // Deliver a matching release even if Shift changed
                            // since the press; local ownership is latched then.
                            terminal.mouse_button(
                                hit.col,
                                hit.row,
                                press.button,
                                false,
                                press.mods,
                            );
                        } else {
                            terminal.mouse_motion(hit.col, hit.row, press.button, mods);
                        }
                    }
                } else if !released {
                    terminal.mouse_motion(hit.col, hit.row, 3, mods);
                }
            }
            _ => {}
        }
        Task::none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_clicks_require_same_pane_cell_and_time_window() {
        let mut mouse = MouseState::default();
        let hit = Hit {
            pane: 1,
            col: 3,
            row: 2,
            side: SelectionSide::Left,
        };
        let now = Instant::now();
        assert_eq!(mouse.click(hit, now), SelectionType::Simple);
        assert_eq!(
            mouse.click(hit, now + Duration::from_millis(100)),
            SelectionType::Semantic
        );
        assert_eq!(
            mouse.click(hit, now + Duration::from_millis(200)),
            SelectionType::Lines
        );
        assert_eq!(
            mouse.click(hit, now + Duration::from_millis(300)),
            SelectionType::Simple
        );
        assert_eq!(
            mouse.click(hit, now + Duration::from_millis(601)),
            SelectionType::Simple
        );
        assert_eq!(
            mouse.click(Hit { pane: 2, ..hit }, now + Duration::from_millis(602)),
            SelectionType::Simple
        );
        assert_eq!(
            mouse.click(
                Hit {
                    pane: 2,
                    col: 4,
                    ..hit
                },
                now + Duration::from_millis(603)
            ),
            SelectionType::Simple
        );
    }
}
