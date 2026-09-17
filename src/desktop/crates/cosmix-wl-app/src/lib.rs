//! A small, widget-free Wayland client runtime.
//!
//! The runtime owns the connection and a calloop loop. It gives the app
//! toplevel and popup surfaces backed by `wl_shm` buffers at physical size,
//! input (keyboard with repeat, pointer, text-input-v3), clipboard and
//! primary selection, and cursor shapes. What goes into the pixels is the
//! app's business: it gets a buffer, draws, and reports what changed.
//!
//! ```no_run
//! use cosmix_wl_app::{App, Ctx, Event, Frame, WindowSpec};
//! struct Demo;
//! impl App for Demo {
//!     fn init(&mut self, cx: &mut Ctx<'_>) {
//!         cx.create_window(WindowSpec::new("demo", (640, 400)));
//!     }
//!     fn event(&mut self, cx: &mut Ctx<'_>, event: Event) {
//!         if let Event::CloseRequested { .. } = event {
//!             cx.exit();
//!         }
//!     }
//!     fn draw(&mut self, _cx: &mut Ctx<'_>, frame: &mut Frame<'_>) {
//!         let (pixels, _, _, _) = frame.buffer_mut();
//!         pixels.fill(0xff);
//!         frame.commit_full();
//!     }
//! }
//! cosmix_wl_app::run(Demo).unwrap();
//! ```

mod clipboard;
pub mod event;
pub mod geom;
pub mod ime;
pub mod pool;
pub mod repeat;
mod runtime;
pub mod scale;
pub mod serial;
mod xkb_state;

/// The calloop the runtime runs on, for sources given to
/// [`Ctx::insert_source`].
pub use calloop;

pub use event::{
    BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, ButtonState, Event, KeyEvent, KeyState, Keysym, Modifiers,
    PointerEvent, PointerKind, ReadStatus, Selection, WindowState,
};
pub use geom::{Damage, Rect};
pub use ime::{ContentHint, ContentPurpose, ImeEvent, ImeState};
pub use runtime::{
    Ctx, Error, Frame, FramePacing, PopupSpec, SourceToken, Stats, Waker, WindowSpec, run,
};
pub use scale::{Scale, SurfaceInfo};
pub use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::Shape as CursorShape;
pub use wayland_protocols::xdg::shell::client::xdg_positioner::{
    Anchor, ConstraintAdjustment, Gravity,
};

/// Identifies a toplevel or popup for the lifetime of the runtime. Ids are
/// never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SurfaceId(u64);

impl SurfaceId {
    pub fn raw(&self) -> u64 {
        self.0
    }

    /// An id not issued by a runtime, for tests of app logic.
    pub fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}

/// The app side of the runtime. All callbacks run on the loop thread.
pub trait App {
    /// Called once the connection is up. Create the first window here.
    fn init(&mut self, cx: &mut Ctx<'_>);

    fn event(&mut self, cx: &mut Ctx<'_>, event: Event);

    /// Called when the surface is configured, a redraw was requested, and no
    /// frame callback is pending. Draw into [`Frame::buffer_mut`] and call
    /// [`Frame::commit_with_damage`]; returning without a commit leaves the
    /// surface unchanged.
    fn draw(&mut self, cx: &mut Ctx<'_>, frame: &mut Frame<'_>);
}
