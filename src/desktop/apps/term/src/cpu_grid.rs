//! D7's second arm: the same grid through iced's CPU rasteriser, so the
//! process holds no DRM fd at all — foot's configuration, which is the only
//! one anyone has ever measured at zero GEM.
//!
//! **This arm costs one full-surface copy per damaged frame that the wgpu arm
//! does not**, and that is not an oversight to fix later: an `iced` image
//! handle owns immutable `Bytes`, so a changed grid is a new handle, and there
//! is no in-place path through the public API. It is honest for what the arm
//! is booked to answer — idle weight (PSS, GEM, threads, binary, idle CPU) —
//! and dishonest for throughput, so do not quote it as a streaming number.
//!
//! The copy is at least made only when the grid actually changed: the handle
//! is keyed on `Frame::generation`, so an idle terminal rebuilds nothing.

use crate::frame::Frame;
use iced::widget::image::{self, Handle};
use std::sync::{Arc, Mutex};

/// Rebuild the cached handle if the frame moved on; otherwise keep it.
pub fn refresh(
    cached: Option<(u64, Handle)>,
    frame: &Arc<Mutex<Frame>>,
) -> Option<(u64, Handle)> {
    let mut frame = frame.lock().expect("frame lock");
    // Bands are the GPU arm's currency; tiny-skia re-blits the whole handle,
    // so this arm consumes them purely to stop them accumulating for the life
    // of the process (cold-review finding, 2026-09-21).
    frame.clear_damage();
    // Emptiness FIRST: a surface cleared by a zero-row screen must drop the
    // handle even if the generation happened to match, or the widget keeps
    // presenting pixels whose source no longer exists.
    let surface = frame.surface();
    if surface.width() == 0 || surface.height() == 0 {
        return None;
    }
    let generation = frame.generation();
    if let Some((cached_generation, _)) = &cached
        && *cached_generation == generation
    {
        return cached;
    }
    let surface = frame.surface();
    Some((
        generation,
        Handle::from_rgba(surface.width(), surface.height(), surface.rgba().to_vec()),
    ))
}

pub fn view(handle: Option<&Handle>) -> image::Image<Handle> {
    // An empty 1x1 stands in until the first repaint, so `view` has the same
    // shape before and after — a `None` arm returning a different widget type
    // would mean two layouts and two chances to get the sizing wrong.
    let handle = handle
        .cloned()
        .unwrap_or_else(|| Handle::from_rgba(1, 1, vec![0, 0, 0, 0]));
    // `content_fit: Fill` because the widget is sized to the grid's LOGICAL
    // extent while the handle carries PHYSICAL pixels: the default `Contain`
    // would letterbox to preserve a ratio that is already exact.
    image::Image::new(handle)
        .filter_method(image::FilterMethod::Nearest)
        .content_fit(iced::ContentFit::Fill)
}
