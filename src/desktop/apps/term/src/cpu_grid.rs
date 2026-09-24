//! D7's second arm: the same grid through iced's CPU rasteriser, so the
//! process holds no DRM fd at all — foot's configuration, which is the only
//! one anyone has ever measured at zero GEM.
//!
//! Each pane paints into the same allocation its image handle shares. Before
//! painting we drop our cached handle and reclaim the `Bytes` with
//! `try_into_mut`. A widget still holding the previous handle forces one copy
//! and a full repaint; otherwise dirty rows are painted in place. At rest
//! only this allocation and iced's premultiplied cache remain. The handle is
//! keyed on `Frame::generation`, so an idle terminal rebuilds nothing.

use crate::frame::Frame;
use bytes::{Bytes, BytesMut};
use cosmix_term_core::raster::{DamageBand, PaintState, Raster};
use cosmix_term_core::terminal::Screen;
use iced::widget::image::{self, Handle};
use std::sync::{Arc, Mutex};

/// CPU-only counterpart of the core's Vec-backed Surface. Keeping the handle
/// here lets painting release ALL app-owned references before reclaiming.
#[derive(Default)]
pub struct Surface {
    state: PaintState,
    rgba: Bytes,
    width: u32,
    height: u32,
    cell: (u32, u32),
    cursor: bool,
    cached: Option<(u64, Handle)>,
    bands: Vec<DamageBand>,
}

impl Surface {
    #[cfg(test)]
    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn is_empty(&self) -> bool {
        self.rgba.is_empty()
    }

    pub fn invalidate(&mut self) {
        self.state.invalidate();
    }

    pub fn paint(&mut self, raster: &mut Raster, screen: &Screen, dirty: &[bool]) -> &[DamageBand] {
        self.bands.clear();
        let (width, height) = raster.target_size(screen);
        if width == 0 || height == 0 {
            self.cached = None;
            self.rgba = Bytes::new();
            self.width = 0;
            self.height = 0;
            self.cursor = false;
            self.invalidate();
            return &self.bands;
        }

        // Avoid acquiring mutable storage for a provably clean frame. This
        // mirrors paint's damage rules, including its conservative treatment
        // of truncated snapshots and its old/current cursor row repaint.
        // Invalidation resets state.grid(), so a raster swap cannot skip.
        let rows = height as usize / raster.height as usize;
        let cursor = screen.cursor_visible && screen.cursor.1 < rows;
        if self.state.grid() == (screen.cols, rows)
            && self.cell == (raster.width, raster.height)
            && (self.width, self.height) == (width, height)
            && screen.rows == rows
            && dirty.len() == rows
            && !dirty.iter().any(|row| *row)
            && !self.cursor
            && !cursor
        {
            return &self.bands;
        }

        self.cached = None;
        let mut pixels = match std::mem::take(&mut self.rgba).try_into_mut() {
            Ok(pixels) => pixels,
            Err(shared) => {
                // A widget may still draw the old handle. Never mutate its
                // bytes, and never let damage skip rows of a fresh buffer.
                self.state.invalidate();
                BytesMut::from(shared.as_ref())
            }
        };
        let len = width as usize * height as usize * 4;
        if pixels.len() != len {
            pixels.resize(len, 0);
            self.state.invalidate();
        }
        self.bands.extend_from_slice(raster.paint(
            screen,
            &mut pixels,
            width as usize * 4,
            &mut self.state,
            dirty,
        ));
        self.rgba = pixels.freeze();
        self.width = width;
        self.height = height;
        self.cell = (raster.width, raster.height);
        self.cursor = cursor && screen.cursor.0 < screen.cols;
        &self.bands
    }
}

/// Rebuild the cached handle if the frame moved on; otherwise keep it.
pub fn refresh(frame: &Arc<Mutex<Frame>>) {
    let mut frame = frame.lock().expect("frame lock");
    // Bands are the GPU arm's currency; tiny-skia re-blits the whole handle,
    // so this arm consumes them purely to stop them accumulating for the life
    // of the process (cold-review finding, 2026-09-21).
    frame.clear_damage();
    // Emptiness FIRST: a surface cleared by a zero-row screen must drop the
    // handle even if the generation happened to match, or the widget keeps
    // presenting pixels whose source no longer exists.
    let generation = frame.generation();
    let surface = frame.cpu_surface_mut();
    if surface.width() == 0 || surface.height() == 0 {
        surface.cached = None;
        return;
    }
    if let Some((cached_generation, _)) = &surface.cached
        && *cached_generation == generation
    {
        return;
    }
    surface.cached = Some((
        generation,
        Handle::from_rgba(surface.width(), surface.height(), surface.rgba.clone()),
    ));
}

pub fn view(frame: &Arc<Mutex<Frame>>) -> image::Image<Handle> {
    // An empty 1x1 stands in until the first repaint, so `view` has the same
    // shape before and after — a `None` arm returning a different widget type
    // would mean two layouts and two chances to get the sizing wrong.
    let handle = frame
        .lock()
        .expect("frame lock")
        .surface()
        .cached
        .as_ref()
        .map(|(_, handle)| handle.clone())
        .unwrap_or_else(|| Handle::from_rgba(1, 1, vec![0, 0, 0, 0]));
    // `content_fit: Fill` because the widget is sized to the grid's LOGICAL
    // extent while the handle carries PHYSICAL pixels: the default `Contain`
    // would letterbox to preserve a ratio that is already exact.
    image::Image::new(handle)
        .filter_method(image::FilterMethod::Nearest)
        .content_fit(iced::ContentFit::Fill)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Painter;
    use cosmix_term_core::config::Cursor;
    use cosmix_term_core::font::FontSize;
    use cosmix_term_core::terminal::Cell;
    use std::time::Instant;

    const PANE: u64 = 1;

    fn painter() -> Painter {
        Painter::new(1.0, FontSize::new(13.0), Cursor::Underline)
            .expect("a monospace font; set TERM_SPIKE_FONT to point at one")
    }

    fn screen() -> Screen {
        Screen {
            cols: 8,
            rows: 4,
            cursor: (0, 0),
            cursor_visible: false,
            cells: (0..32)
                .map(|_| Cell {
                    c: 'M',
                    fg: [200, 200, 200],
                    bg: [10, 20, 30],
                    bold: false,
                })
                .collect(),
            updated: Instant::now(),
        }
    }

    fn handle(frame: &Arc<Mutex<Frame>>) -> Handle {
        frame
            .lock()
            .unwrap()
            .surface()
            .cached
            .as_ref()
            .unwrap()
            .1
            .clone()
    }

    fn pixels(handle: &Handle) -> &Bytes {
        let Handle::Rgba { pixels, .. } = handle else {
            panic!("expected an RGBA handle");
        };
        pixels
    }

    #[test]
    fn successive_generations_reclaim_the_same_allocation() {
        let mut painter = painter();
        let frame = painter.frame(PANE);
        let mut grid = screen();
        assert!(painter.repaint(PANE, &grid, &[]));
        refresh(&frame);
        let old = handle(&frame);
        let pointer = pixels(&old).as_ptr();
        let id = old.id();
        let before = pixels(&old).to_vec();
        assert_eq!(frame.lock().unwrap().surface().rgba.as_ptr(), pointer);
        drop(old);

        // Only row 1 is dirty. Changing the snapshot's other rows as well
        // makes a mistaken full repaint observable in this reuse case.
        for cell in &mut grid.cells {
            cell.bg = [40, 50, 60];
        }
        assert!(painter.repaint(PANE, &grid, &[false, true, false, false]));
        refresh(&frame);
        let next = handle(&frame);
        assert_eq!(pixels(&next).as_ptr(), pointer);
        assert_ne!(next.id(), id, "new pixels need a new iced cache entry");
        assert_eq!(frame.lock().unwrap().generation(), 2);
        let row_bytes = painter.cell().0 as usize * grid.cols * 4 * painter.cell().1 as usize;
        assert_eq!(&pixels(&next)[..row_bytes], &before[..row_bytes]);
        assert_ne!(
            &pixels(&next)[row_bytes..2 * row_bytes],
            &before[row_bytes..2 * row_bytes]
        );
        assert_eq!(&pixels(&next)[2 * row_bytes..], &before[2 * row_bytes..]);
    }

    #[test]
    fn outstanding_handle_gets_a_fresh_buffer_and_full_repaint() {
        let mut painter = painter();
        let frame = painter.frame(PANE);
        let mut grid = screen();
        assert!(painter.repaint(PANE, &grid, &[]));
        refresh(&frame);
        let old = handle(&frame);
        let before = pixels(&old).to_vec();
        for cell in &mut grid.cells {
            cell.bg = [70, 80, 90];
        }
        assert!(painter.repaint(PANE, &grid, &[false, true, false, false]));
        // Inspect bands before refresh consumes them: fallback owes every row.
        assert_eq!(
            frame.lock().unwrap().take_damage(),
            vec![DamageBand {
                y: 0,
                height: painter.cell().1 * grid.rows as u32,
            }]
        );
        refresh(&frame);
        let next = handle(&frame);
        assert_ne!(pixels(&old).as_ptr(), pixels(&next).as_ptr());
        assert_eq!(
            pixels(&old).as_ref(),
            before.as_slice(),
            "old widget pixels changed"
        );
        assert_ne!(old.id(), next.id());

        let mut reference = cosmix_term_core::raster::Surface::default();
        let mut raster = Raster::new(1.0, 13.0, Cursor::Underline).expect("a monospace font");
        raster.render_into(&grid, &[], &mut reference);
        assert_eq!(pixels(&next).as_ref(), reference.rgba());
    }

    #[test]
    fn idle_keeps_the_handle_and_buffer_even_with_a_widget_clone() {
        let mut painter = painter();
        let frame = painter.frame(PANE);
        let grid = screen();
        assert!(painter.repaint(PANE, &grid, &[]));
        refresh(&frame);
        let old = handle(&frame);
        let before = pixels(&old).to_vec();
        let generation = frame.lock().unwrap().generation();
        assert!(!painter.repaint(PANE, &grid, &[false; 4]));
        refresh(&frame);
        refresh(&frame);
        let next = handle(&frame);
        assert_eq!(next.id(), old.id());
        assert_eq!(pixels(&next).as_ptr(), pixels(&old).as_ptr());
        assert_eq!(pixels(&next).as_ref(), before.as_slice());
        assert_eq!(frame.lock().unwrap().generation(), generation);
    }

    #[test]
    fn idle_guard_matches_core_cursor_and_geometry_damage() {
        let mut painter = painter();
        let frame = painter.frame(PANE);
        let mut raster = Raster::new(1.0, 13.0, Cursor::Underline).expect("a monospace font");
        let mut reference = cosmix_term_core::raster::Surface::default();
        let mut check = |grid: &Screen, dirty: &[bool]| {
            let changed = !raster.render_into(grid, dirty, &mut reference).is_empty();
            assert_eq!(painter.repaint(PANE, grid, dirty), changed);
            refresh(&frame);
            let current = handle(&frame);
            assert_eq!(pixels(&current).as_ref(), reference.rgba());
        };
        let mut grid = screen();
        check(&grid, &[]);
        grid.cursor_visible = true;
        check(&grid, &[false; 4]);
        grid.cursor = (2, 2);
        check(&grid, &[false; 4]);
        grid.cursor_visible = false;
        check(&grid, &[false; 4]);
        check(&grid, &[false; 4]);
        grid.cursor_visible = true;
        grid.cursor.0 = grid.cols; // A visible cursor outside the columns is not drawn.
        check(&grid, &[false; 4]);
        grid.cursor_visible = false;
        check(&grid, &[false; 4]);
        check(&grid, &[false; 3]); // Malformed damage means repaint everything.
        grid.cells.truncate(16); // Only two rows are paintable.
        check(&grid, &[false; 4]);
        check(&grid, &[false; 4]);
        grid.cols = 4;
        check(&grid, &[false; 4]);
    }
}
