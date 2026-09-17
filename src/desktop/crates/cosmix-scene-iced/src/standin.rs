//! A renderer that exercises the bridge until the iced host lands: a solid
//! panel, a hover square under the pointer, and a caret that blinks only
//! while the surface has keyboard focus.

use std::time::Duration;

use crate::surface::{
    CursorIcon, ImeEvent, ImeRequest, Key, NamedKey, Processed, Rect, SurfaceEvent, SurfaceRenderer,
};

const PANEL: [u8; 4] = [0x20, 0x24, 0x2c, 0xff];
const HOVER: [u8; 4] = [0x3a, 0x5f, 0x9e, 0xff];
const CARET: [u8; 4] = [0xe8, 0xe8, 0xe8, 0xff];
pub const BLINK: Duration = Duration::from_millis(530);

#[derive(Default)]
pub struct StandIn {
    width: u32,
    height: u32,
    scale: f32,
    hover: Option<Rect>,
    focused: bool,
    caret_on: bool,
    next_blink: Option<Duration>,
    /// Characters typed (commits included); moves the caret.
    column: u32,
    damage: Vec<Rect>,
}

impl StandIn {
    fn unit(&self) -> u32 {
        (8.0 * self.scale.max(1.0)).round() as u32
    }
    fn hover_at(&self, x: f32, y: f32) -> Rect {
        let side = 3 * self.unit();
        let half = side as f32 / 2.0;
        Rect::new(
            (x - half).max(0.0) as u32,
            (y - half).max(0.0) as u32,
            side,
            side,
        )
    }
    fn caret(&self) -> Rect {
        let unit = self.unit();
        Rect::new(unit + self.column * unit, unit, (unit / 4).max(1), 2 * unit)
    }
    fn damage(&mut self, rect: Rect) {
        if let Some(rect) = rect.clip(self.width, self.height) {
            self.damage.push(rect);
        }
    }
    fn move_caret(&mut self, column: u32) {
        if column != self.column {
            self.damage(self.caret());
            self.column = column;
            self.damage(self.caret());
        }
    }
    fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let caret = self.caret();
        if self.focused && self.caret_on && caret.contains(x as f32, y as f32) {
            CARET
        } else if self.hover.is_some_and(|h| h.contains(x as f32, y as f32)) {
            HOVER
        } else {
            PANEL
        }
    }
}

impl SurfaceRenderer for StandIn {
    fn resize(&mut self, width: u32, height: u32, scale: f32) {
        self.width = width;
        self.height = height;
        self.scale = scale;
        self.damage.clear();
        self.damage.push(Rect::new(0, 0, width, height));
    }

    fn queue(&mut self, event: SurfaceEvent) {
        match event {
            SurfaceEvent::PointerMoved { x, y } => {
                let next = self.hover_at(x, y);
                if self.hover != Some(next) {
                    if let Some(old) = self.hover.replace(next) {
                        self.damage(old);
                    }
                    self.damage(next);
                }
            }
            SurfaceEvent::PointerLeft => {
                if let Some(old) = self.hover.take() {
                    self.damage(old);
                }
            }
            SurfaceEvent::Focus(focused) if focused != self.focused => {
                self.focused = focused;
                self.caret_on = focused;
                // Armed by the next `process`, which knows the time.
                self.next_blink = None;
                self.damage(self.caret());
            }
            SurfaceEvent::Key {
                key, pressed: true, ..
            } if self.focused => match key {
                Key::Named(NamedKey::Backspace) => self.move_caret(self.column.saturating_sub(1)),
                Key::Character(text) => self.move_caret(self.column + text.chars().count() as u32),
                _ => {}
            },
            SurfaceEvent::Ime(ImeEvent::Commit(text)) if self.focused => {
                self.move_caret(self.column + text.chars().count() as u32)
            }
            _ => {}
        }
    }

    fn process(&mut self, now: Duration) -> Processed {
        if self.focused {
            match self.next_blink {
                None => self.next_blink = Some(now + BLINK),
                Some(at) if now >= at => {
                    self.caret_on = !self.caret_on;
                    self.damage(self.caret());
                    self.next_blink = Some(now + BLINK);
                }
                Some(_) => {}
            }
        }
        Processed {
            needs_redraw: !self.damage.is_empty(),
            cursor: if self.hover.is_some() {
                CursorIcon::Text
            } else {
                CursorIcon::Default
            },
            ime: if self.focused {
                ImeRequest::Enabled {
                    cursor: self.caret(),
                }
            } else {
                ImeRequest::Disabled
            },
            wake_at: self.focused.then_some(self.next_blink).flatten(),
        }
    }

    fn draw(&mut self, buffer: &mut [u8], width: u32, height: u32, stride: u32) -> Vec<Rect> {
        let damage = std::mem::take(&mut self.damage);
        let mut drawn = Vec::with_capacity(damage.len());
        for rect in damage {
            let Some(rect) = rect.clip(width, height) else {
                continue;
            };
            for y in rect.y..rect.bottom() {
                let row = (y * stride) as usize;
                for x in rect.x..rect.right() {
                    let at = row + x as usize * 4;
                    buffer[at..at + 4].copy_from_slice(&self.pixel(x, y));
                }
            }
            drawn.push(rect);
        }
        drawn
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drawn(renderer: &mut StandIn) -> Vec<Rect> {
        let mut buffer = vec![0; 200 * 100 * 4];
        renderer.draw(&mut buffer, 200, 100, 800)
    }

    #[test]
    fn caret_blinks_only_while_focused() {
        let mut renderer = StandIn::default();
        renderer.resize(200, 100, 1.0);
        drawn(&mut renderer);
        for step in 0..10 {
            let processed = renderer.process(BLINK * step);
            assert!(!processed.needs_redraw && processed.wake_at.is_none());
        }
        renderer.queue(SurfaceEvent::Focus(true));
        let processed = renderer.process(Duration::ZERO);
        assert_eq!(processed.wake_at, Some(BLINK));
        assert_eq!(drawn(&mut renderer), vec![renderer.caret()]);
        assert!(renderer.process(BLINK).needs_redraw);
        assert_eq!(drawn(&mut renderer), vec![renderer.caret()]);
        renderer.queue(SurfaceEvent::Focus(false));
        let processed = renderer.process(BLINK * 2);
        assert!(processed.needs_redraw);
        assert_eq!(processed.ime, ImeRequest::Disabled);
        drawn(&mut renderer);
        assert!(renderer.process(BLINK * 20).wake_at.is_none());
    }
}
