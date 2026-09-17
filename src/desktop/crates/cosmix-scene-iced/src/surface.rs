//! The renderer seam. Nothing here depends on Bevy, so an iced host (or any
//! other CPU renderer) can be adapted to it without joining a Bevy build.
//!
//! Coordinates are physical pixels of the surface, origin top-left, unless a
//! field says otherwise. Time is the host's monotonic clock as a `Duration`
//! since an arbitrary epoch (Quoin passes `Time<Real>::elapsed()`).

use std::time::Duration;

use cosmix_scene::ResolvedScene;

/// A pixel rectangle in surface coordinates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub const fn new(x: u32, y: u32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }
    pub const fn right(&self) -> u32 {
        self.x.saturating_add(self.w)
    }
    pub const fn bottom(&self) -> u32 {
        self.y.saturating_add(self.h)
    }
    pub const fn area(&self) -> u64 {
        self.w as u64 * self.h as u64
    }
    pub const fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }
    /// The part of `self` inside a `width` x `height` surface.
    pub fn clip(&self, width: u32, height: u32) -> Option<Self> {
        let right = self.right().min(width);
        let bottom = self.bottom().min(height);
        (self.x < right && self.y < bottom).then(|| Self {
            x: self.x,
            y: self.y,
            w: right - self.x,
            h: bottom - self.y,
        })
    }
    pub fn union(&self, other: &Self) -> Self {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        Self {
            x,
            y,
            w: self.right().max(other.right()) - x,
            h: self.bottom().max(other.bottom()) - y,
        }
    }
    /// True when the rectangles overlap or share an edge.
    pub fn touches(&self, other: &Self) -> bool {
        self.x <= other.right()
            && other.x <= self.right()
            && self.y <= other.bottom()
            && other.y <= self.bottom()
    }
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x as f32
            && y >= self.y as f32
            && x < self.right() as f32
            && y < self.bottom() as f32
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
    pub logo: bool,
    /// Lock states, which a host that carries its own seat modifiers can
    /// report; a host reading them from `ButtonInput` leaves them false.
    pub caps_lock: bool,
    pub num_lock: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerButton {
    Primary,
    Secondary,
    Middle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollUnit {
    Line,
    Pixel,
}

/// The named keys iced's text and navigation widgets act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NamedKey {
    Enter,
    Tab,
    Space,
    Backspace,
    Delete,
    Escape,
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    ArrowDown,
    Home,
    End,
    PageUp,
    PageDown,
    Shift,
    Control,
    Alt,
    Super,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Key {
    Named(NamedKey),
    Character(String),
    Unidentified,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ImeEvent {
    /// Composition text and its byte-range cursor; empty text ends composition.
    Preedit {
        text: String,
        cursor: Option<(usize, usize)>,
    },
    Commit(String),
    /// The input method went away; drop any composition.
    Disabled,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SurfaceEvent {
    PointerMoved {
        x: f32,
        y: f32,
    },
    PointerLeft,
    PointerButton {
        button: PointerButton,
        pressed: bool,
    },
    Scroll {
        unit: ScrollUnit,
        x: f32,
        y: f32,
    },
    Key {
        key: Key,
        /// The layout-independent Latin letter or digit of the physical key,
        /// for shortcut matching on non-Latin layouts.
        latin: Option<char>,
        text: Option<String>,
        pressed: bool,
        repeat: bool,
        modifiers: Modifiers,
    },
    Modifiers(Modifiers),
    /// The surface gained or lost keyboard focus.
    Focus(bool),
    Ime(ImeEvent),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorIcon {
    #[default]
    Default,
    Pointer,
    Text,
    Grab,
    Grabbing,
    NotAllowed,
    ResizeHorizontal,
    ResizeVertical,
}

/// What the renderer wants from the input method.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum ImeRequest {
    #[default]
    Disabled,
    Enabled {
        /// The caret, in surface physical pixels.
        cursor: Rect,
    },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Processed {
    /// `draw` has damage to produce.
    pub needs_redraw: bool,
    pub cursor: CursorIcon,
    pub ime: ImeRequest,
    /// The next time `process` must run even without input (caret blink,
    /// animation). `None` means the renderer is idle until the next event.
    pub wake_at: Option<Duration>,
}

/// A CPU renderer owning one scene surface.
///
/// The host calls, per update: `queue` for each routed event, then `process`,
/// then `draw` when `process` asked for it (or after `resize`). `draw` writes
/// only damaged pixels into the caller-owned buffer, which keeps its contents
/// between calls, and returns the rectangles it changed. The buffer is RGBA8,
/// premultiplied alpha, sRGB-encoded, `stride` bytes per row.
pub trait SurfaceRenderer {
    /// Physical size and scale factor. The next `draw` must repaint everything.
    fn resize(&mut self, width: u32, height: u32, scale: f32);
    /// A new accepted scene revision.
    fn set_scene(&mut self, _scene: &ResolvedScene) {}
    fn queue(&mut self, event: SurfaceEvent);
    fn process(&mut self, now: Duration) -> Processed;
    fn draw(&mut self, buffer: &mut [u8], width: u32, height: u32, stride: u32) -> Vec<Rect>;
}
