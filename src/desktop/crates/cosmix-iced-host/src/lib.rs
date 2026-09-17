//! Headless iced 0.14 host.
//!
//! A [`Surface`] runs one iced program without winit and rasterises it with
//! `iced_tiny_skia` into a buffer the caller owns. The crate knows nothing of
//! Wayland or Bevy: a host feeds [`iced_core::Event`]s, calls
//! [`Surface::process`] when input arrives and [`Surface::draw`] when it wants
//! a frame, and acts on the returned [`Requests`] (cursor shape, input method,
//! next redraw). No threads or timers are created here.
//!
//! Only damaged pixels are rewritten; the buffer must keep its previous
//! contents between draws or the host must call [`Surface::invalidate`].

mod clipboard;
mod damage;
mod fonts;
mod ime;
pub mod input;
#[cfg(feature = "xkb")]
pub mod keys;
mod surface;

pub use clipboard::{Clipboard, ClipboardKind, MemoryClipboard, NullClipboard};
pub use damage::{DamageRect, PixelFormat};
pub use fonts::{load_font, load_font_file, loaded_face_count};
pub use ime::ImeRequest;
pub use surface::{DrawError, Frame, Redraw, Requests, Settings, Surface, Update};

pub use iced_core as core;
pub use iced_runtime as runtime;
pub use iced_tiny_skia as tiny_skia_renderer;
pub use iced_widget as widget;

/// The renderer every hosted program draws with.
pub type Renderer = iced_tiny_skia::Renderer;

/// The theme every hosted program draws with.
pub type Theme = iced_core::Theme;

/// A widget tree produced by [`Program::view`].
pub type Element<'a, Message> = iced_core::Element<'a, Message, Theme, Renderer>;

/// An iced application driven by a [`Surface`].
///
/// There is no `Task` return: effects a program wants from its host go
/// through the host's own channels, and widget operations (focus, scroll)
/// are applied with [`Surface::operate`].
pub trait Program {
    type Message;

    fn update(&mut self, message: Self::Message);

    fn view(&self) -> Element<'_, Self::Message>;
}
