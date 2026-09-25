//! D7's second arm: the same grid through iced's CPU rasteriser, so the
//! process holds no DRM fd at all — foot's configuration, which is the only
//! one anyone has ever measured at zero GEM.
//!
//! Each four-row band paints native BGRA into its generation-owned allocation. Before
//! painting we drop our cached handle and reclaim the `Bytes` with
//! `try_into_mut`. iced's renderer layers and compositor history retain old
//! handles, normally forcing one memcpy plus incremental dirty-row painting.
//! Reclaim remains zero-copy when no other owner remains. At rest each pane
//! has one app-side buffer per band (the separate Frame Vec is gone), plus iced's
//! retained older buffers after a burst, until later redraws release them.
//! There is no RGBA image or converted image cache. Unchanged bands retain
//! their native generation, bounding damage to changed bands.

use crate::frame::Frame;
use bytes::{Bytes, BytesMut};
use cosmix_term_core::raster::{DamageBand, PaintState, PixelFormat, Raster};
use cosmix_term_core::terminal::Screen;
use iced_tiny_skia::grid::Grid as Handle;
use std::sync::{Arc, Mutex};

#[cfg(test)]
#[path = "cpu_bench.rs"]
mod bench;

#[path = "cpu_bands.rs"]
mod bands;
pub use bands::Surface;
#[path = "cpu_widget.rs"]
mod widget;
pub use widget::{Grid, view};

/// CPU-only counterpart of the core's Vec-backed Surface. Keeping the handle
/// here lets painting release ALL app-owned references before reclaiming.
#[derive(Default)]
pub(super) struct PixelBand {
    state: PaintState,
    native: Bytes,
    width: u32,
    height: u32,
    cell: (u32, u32),
    cursor: bool,
    cached: Option<(u64, Handle)>,
    bands: Vec<DamageBand>,
}

impl PixelBand {
    #[cfg(test)]
    pub fn width(&self) -> u32 {
        self.width
    }

    #[cfg(test)]
    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn is_empty(&self) -> bool {
        self.native.is_empty()
    }

    pub fn invalidate(&mut self) {
        self.state.invalidate();
    }

    fn paint_inner(
        &mut self,
        raster: &mut Raster,
        screen: &Screen,
        dirty: &[bool],
        discard: bool,
    ) -> &[DamageBand] {
        self.bands.clear();
        let (width, height) = raster.target_size(screen);
        if width == 0 || height == 0 {
            self.cached = None;
            self.native = Bytes::new();
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
        // Lifetime rule: a widget, renderer layer or age-repair history may
        // retain ANY older generation. Never mutate its bytes, even after two
        // frames. Bytes grants mutation only to the sole owner; otherwise the
        // new generation gets separate storage. No fixed-age assumption.
        let mut pixels = match std::mem::take(&mut self.native).try_into_mut() {
            Ok(pixels) => pixels,
            Err(shared) => {
                if discard {
                    // Every row will be overwritten: copying retained pixels
                    // here only wastes memory bandwidth on full-screen TUIs.
                    self.state.invalidate();
                    BytesMut::zeroed(shared.len())
                } else {
                    // iced may still draw the old handle. Copy every byte and
                    // preserve damage state: unchanged rows already contain the
                    // right pixels, even though their address has changed.
                    let pixels = BytesMut::from(shared.as_ref());
                    self.state.rebind(&pixels);
                    pixels
                }
            }
        };
        let len = width as usize * height as usize * 4;
        if pixels.len() != len {
            pixels.resize(len, 0);
            self.state.invalidate();
        }
        self.bands.extend_from_slice(raster.paint_format(
            screen,
            &mut pixels,
            width as usize * 4,
            &mut self.state,
            dirty,
            PixelFormat::Bgra,
        ));
        self.native = pixels.freeze();
        self.width = width;
        self.height = height;
        self.cell = (raster.width, raster.height);
        self.cursor = cursor && screen.cursor.0 < screen.cols;
        &self.bands
    }

    pub fn cache_handle(&mut self, generation: u64) {
        if self.is_empty() {
            self.cached = None;
        } else if self.cached.as_ref().map(|(current, _)| *current) != Some(generation) {
            self.cached = Some((
                generation,
                Handle::new(self.width, self.height, self.native.clone())
                    .expect("painted native band dimensions"),
            ));
        }
    }
}

/// Rebuild the cached handle if the frame moved on; otherwise keep it.
pub fn refresh(frame: &Arc<Mutex<Frame>>) {
    let mut frame = frame.lock().expect("frame lock");
    // Per-band handles already record CPU damage; drain the frame's separate
    // upload bookkeeping so repeated PTY reads cannot accumulate it.
    frame.clear_damage();
    // Surface::paint drops the tiles on clear, regardless of generation.
    let generation = frame.generation();
    frame.cpu_surface_mut().cache_handle(generation);
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
        frame.lock().unwrap().surface().tiles[0]
            .cached
            .as_ref()
            .unwrap()
            .1
            .clone()
    }

    fn pixels(handle: &Handle) -> &Bytes {
        handle.pixels()
    }

    fn native(rgba: &[u8]) -> Vec<u8> {
        rgba.chunks_exact(4)
            .flat_map(|p| [p[2], p[1], p[0], p[3]])
            .collect()
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
        let id = old.generation();
        let before = pixels(&old).to_vec();
        assert_eq!(
            frame.lock().unwrap().surface().tiles[0].native.as_ptr(),
            pointer
        );
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
        assert_ne!(
            next.generation(),
            id,
            "new pixels need a new damage generation"
        );
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
    fn outstanding_handle_gets_a_fresh_buffer_and_incremental_repaint() {
        let mut painter = painter();
        let frame = painter.frame(PANE);
        let mut grid = screen();
        assert!(painter.repaint(PANE, &grid, &[]));
        refresh(&frame);
        let old = handle(&frame);
        let before = pixels(&old).to_vec();
        for cell in &mut grid.cells[8..16] {
            cell.bg = [70, 80, 90];
        }
        assert!(painter.repaint(PANE, &grid, &[false, true, false, false]));
        // Inspect bands before refresh consumes them: only row 1 is owed.
        assert_eq!(
            frame.lock().unwrap().take_damage(),
            vec![DamageBand {
                y: painter.cell().1,
                height: painter.cell().1,
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
        assert_ne!(old.generation(), next.generation());

        let mut reference = cosmix_term_core::raster::Surface::default();
        let mut raster = Raster::new(1.0, 13.0, Cursor::Underline).expect("a monospace font");
        raster.render_into(&grid, &[], &mut reference);
        assert_eq!(pixels(&next).as_ref(), native(reference.rgba()));
    }

    #[test]
    fn retained_layers_keep_incremental_paint_without_app_buffer_history() {
        let mut painter = painter();
        let frame = painter.frame(PANE);
        let mut grid = screen();
        assert!(painter.repaint(PANE, &grid, &[]));
        refresh(&frame);
        let mut history = Vec::new();
        let mut raster = Raster::new(1.0, 13.0, Cursor::Underline).expect("a monospace font");

        for generation in 2..=9 {
            // Keep each presented handle, including the first, across later
            // generations as iced's layers/compositor history do.
            let old = handle(&frame);
            let before = pixels(&old).to_vec();
            history.push((old, before));
            let row = generation as usize % grid.rows;
            for cell in &mut grid.cells[row * grid.cols..(row + 1) * grid.cols] {
                cell.bg = [generation as u8 * 20, 80, 90];
            }
            let mut dirty = [false; 4];
            dirty[row] = true;
            assert!(painter.repaint(PANE, &grid, &dirty));
            assert_eq!(
                frame.lock().unwrap().take_damage(),
                vec![DamageBand {
                    y: row as u32 * painter.cell().1,
                    height: painter.cell().1,
                }]
            );
            refresh(&frame);
            let current = handle(&frame);
            let mut reference = cosmix_term_core::raster::Surface::default();
            raster.render_into(&grid, &[], &mut reference);
            assert_eq!(pixels(&current).as_ref(), native(reference.rgba()));
            let locked = frame.lock().unwrap();
            assert_eq!(locked.generation(), generation);
            assert_eq!(
                locked.surface().tiles[0].native.as_ptr(),
                pixels(&current).as_ptr()
            );
            assert_eq!(
                locked.surface().tiles[0].native.len(),
                pixels(&current).len()
            );
            for (retained, expected) in &history {
                assert_eq!(pixels(retained).as_ref(), expected.as_slice());
                assert_ne!(pixels(retained).as_ptr(), pixels(&current).as_ptr());
            }
        }

        // With the app and its current cache still alive, releasing simulated
        // renderer history leaves each older buffer uniquely owned. The app
        // has retained no previous-generation buffers of its own.
        for (retained, _) in history {
            let pixels = retained.into_pixels();
            assert!(pixels.try_into_mut().is_ok(), "app retained an old buffer");
        }
    }

    #[test]
    fn no_damage_after_cache_release_restores_current_generation_without_refresh() {
        let mut painter = painter();
        let frame = painter.frame(PANE);
        let grid = screen();
        assert!(painter.repaint(PANE, &grid, &[]));
        refresh(&frame);
        let old = handle(&frame);
        let generation = frame.lock().unwrap().generation();
        // Force the conservative frontend guard to fall through while core
        // state correctly reports no cursor and no dirty rows. This models
        // divergence between the guard and core's damage rules.
        frame.lock().unwrap().cpu_surface_mut().tiles[0].cursor = true;
        assert!(!painter.repaint(PANE, &grid, &[false; 4]));
        // Deliberately do not refresh: main skips it for a false repaint.
        let current = handle(&frame);
        assert_eq!(pixels(&current).as_ref(), pixels(&old).as_ref());
        let locked = frame.lock().unwrap();
        let surface = &locked.surface().tiles[0];
        assert_eq!(locked.generation(), generation);
        assert_eq!(surface.cached.as_ref().unwrap().0, generation);
        assert_eq!(surface.native.as_ptr(), pixels(&current).as_ptr());
        let (width, height) = (current.width(), current.height());
        assert_eq!((width, height), (surface.width(), surface.height()));
        assert!(width > 1 && height > 1);
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
        assert_eq!(next.generation(), old.generation());
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
            assert_eq!(pixels(&current).as_ref(), native(reference.rgba()));
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
