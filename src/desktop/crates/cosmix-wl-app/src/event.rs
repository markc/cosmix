//! Events delivered to the app.

use crate::SurfaceId;
use crate::ime::ImeEvent;
use crate::scale::SurfaceInfo;
pub use smithay_client_toolkit::seat::keyboard::Keysym;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub logo: bool,
    pub caps_lock: bool,
    pub num_lock: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyState {
    Pressed,
    Released,
    Repeated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyEvent {
    /// The surface with keyboard focus.
    pub surface: Option<SurfaceId>,
    pub state: KeyState,
    pub keysym: Keysym,
    /// The evdev key code (without xkb's +8 offset).
    pub raw_code: u32,
    /// Text the key produces, after compose. `None` on release.
    pub text: Option<String>,
    pub modifiers: Modifiers,
    /// Compositor timestamp in milliseconds (synthesised for repeats).
    pub time: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonState {
    Pressed,
    Released,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PointerKind {
    Enter,
    Leave,
    Motion,
    /// `button` is the evdev code (`BTN_LEFT` = 0x110).
    Button {
        button: u32,
        state: ButtonState,
    },
    Axis {
        /// Continuous scroll in logical pixels.
        horizontal: f64,
        vertical: f64,
        /// Wheel steps in 1/120ths. wl_seat is bound at v7, so this is
        /// derived from `axis_discrete` (one step = 120).
        horizontal_120: i32,
        vertical_120: i32,
        stop: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PointerEvent {
    pub surface: SurfaceId,
    /// Surface-local logical position.
    pub position: (f64, f64),
    pub kind: PointerKind,
    pub modifiers: Modifiers,
}

pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Selection {
    /// `wl_data_device` (Ctrl+C / Ctrl+V).
    Clipboard,
    /// `zwp_primary_selection` (select / middle click).
    Primary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WindowState {
    pub maximized: bool,
    pub fullscreen: bool,
    pub activated: bool,
    pub resizing: bool,
    /// The compositor draws the decorations.
    pub server_decorations: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A toplevel was configured. The first configure precedes the first
    /// draw; `info` may change size or scale on later ones.
    Configure {
        surface: SurfaceId,
        info: SurfaceInfo,
        state: WindowState,
        first: bool,
    },
    /// A popup was configured (placed). `position` is relative to the
    /// parent; `repositioned` carries the token of a reposition request.
    PopupConfigure {
        surface: SurfaceId,
        info: SurfaceInfo,
        position: (i32, i32),
        first: bool,
        repositioned: Option<u32>,
    },
    /// The surface's scale changed; buffers are reallocated at the new
    /// physical size and the next draw is a full one.
    ScaleChanged {
        surface: SurfaceId,
        info: SurfaceInfo,
    },
    /// The user asked to close a toplevel. The runtime does nothing else.
    CloseRequested {
        surface: SurfaceId,
    },
    /// The compositor dismissed a popup (click outside, grab broken). The
    /// popup and any popups above it are already destroyed.
    PopupDone {
        surface: SurfaceId,
    },
    KeyboardFocus {
        surface: SurfaceId,
        focused: bool,
    },
    Key(KeyEvent),
    Modifiers(Modifiers),
    Pointer(PointerEvent),
    /// Input method results for `surface`.
    Ime {
        surface: Option<SurfaceId>,
        event: ImeEvent,
    },
    /// Result of [`crate::Ctx::request_selection`]. `None` when the
    /// selection is empty, not text, or could not be read.
    SelectionText {
        selection: Selection,
        text: Option<String>,
    },
    /// Another client took a selection this app had set.
    SelectionLost {
        selection: Selection,
    },
    /// Delivered for [`crate::Waker::wake`].
    Wake(u64),
    /// A one-shot timer set with [`crate::Ctx::set_timer`] fired.
    Timer(u64),
    /// Another client set `selection`; request it to read the text.
    SelectionChanged {
        selection: Selection,
    },
}
