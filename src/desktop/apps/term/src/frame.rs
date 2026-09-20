//! The one grid image, and the only thing the renderer and the VT loop share.
//!
//! A [`Frame`] is created once per pane and lives for the pane's life. The VT
//! loop rasterises into it in place and appends the damaged bands; the
//! renderer uploads those bands and clears them. Neither side ever allocates
//! a grid-sized buffer per frame, which is the requirement this whole
//! frontend exists to meet: the Bevy terminal's `Image::new`-per-damaged-frame
//! is where 320 MB of its 344 MB of mapped GEM went
//! (`_journal/2026-09-20-term-vs-foot-memory-anatomy.md`).

use cosmix_term_core::raster::{DamageBand, Raster, Surface};
use cosmix_term_core::terminal::Screen;
use std::sync::{Arc, Mutex};

/// The grid image plus the regions of it nobody has presented yet.
#[derive(Default)]
pub struct Frame {
    surface: Surface,
    /// Damage accumulated since the last [`Frame::take_damage`]. It is a list
    /// rather than a single rect because a burst of PTY output can rasterise
    /// several times between two presented frames, and the union of two
    /// distant rows is most of the screen.
    damage: Vec<DamageBand>,
    /// Bumped on every render that wrote anything. The CPU arm keys its
    /// image-handle cache on it; tests use it to tell a real repaint from a
    /// no-op.
    generation: u64,
}

impl Frame {
    pub fn surface(&self) -> &Surface {
        &self.surface
    }

    // Read by the CPU arm's handle cache and by the tests; the wgpu arm keys
    // its uploads on damage bands instead, so it never asks.
    #[cfg_attr(feature = "wgpu", allow(dead_code))]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Damage since the last call, coalesced, and cleared.
    // The CPU arm uploads nothing — tiny-skia blits the whole handle — so only
    // the wgpu arm and the tests consume bands.
    #[cfg_attr(not(feature = "wgpu"), allow(dead_code))]
    pub fn take_damage(&mut self) -> Vec<DamageBand> {
        coalesce(std::mem::take(&mut self.damage))
    }

    /// Drop pending damage: the wgpu arm calls it when it is about to upload
    /// the whole surface anyway, the CPU arm on every rebuilt handle.
    pub fn clear_damage(&mut self) {
        self.damage.clear();
    }
}

/// Merges overlapping and touching bands so a burst of row-damage costs one
/// upload per contiguous region instead of one per render.
fn coalesce(mut bands: Vec<DamageBand>) -> Vec<DamageBand> {
    if bands.len() < 2 {
        return bands;
    }
    bands.sort_unstable_by_key(|band| band.y);
    let mut merged: Vec<DamageBand> = Vec::with_capacity(bands.len());
    for band in bands {
        match merged.last_mut() {
            Some(last) if band.y <= last.y + last.height => {
                let end = (band.y + band.height).max(last.y + last.height);
                last.height = end - last.y;
            }
            _ => merged.push(band),
        }
    }
    merged
}

/// The app-side half: the glyph cache, and the handle to the shared frame.
///
/// The [`Raster`] deliberately does NOT live inside the shared `Frame`. The
/// renderer needs the pixels and nothing else, and a swash `ScaleContext`
/// behind the same lock the GPU thread takes would make every glyph miss
/// contend with every upload.
pub struct Painter {
    raster: Raster,
    frame: Arc<Mutex<Frame>>,
}

impl Painter {
    pub fn new(raster: Raster) -> Self {
        Self {
            raster,
            frame: Arc::new(Mutex::new(Frame::default())),
        }
    }

    pub fn frame(&self) -> Arc<Mutex<Frame>> {
        self.frame.clone()
    }

    /// Physical cell size, for turning a window into a column count.
    pub fn cell(&self) -> (u32, u32) {
        (self.raster.width, self.raster.height)
    }

    /// Logical (scale-divided) cell size — the units iced lays out in.
    pub fn logical_cell(&self) -> (f32, f32) {
        (self.raster.logical_width(), self.raster.logical_height())
    }

    pub fn scale(&self) -> f32 {
        self.raster.scale
    }

    /// Swap in a raster built for a new scale or font size, and force a full
    /// repaint.
    ///
    /// The invalidation is NOT belt-and-braces. `render_into` decides "is a
    /// full repaint owed?" from the surface's recorded cell size, and two
    /// different font sizes can round to the same integer cell — 13.0 px and
    /// 12.9 px both give an 8x16 DejaVuSansMono cell, with visibly different
    /// glyphs inside it (cold-review finding, 2026-09-21, reproduced). The
    /// swap is the only place that knows the raster changed, so it is the
    /// only place that can say so.
    pub fn replace_raster(&mut self, raster: Raster) {
        self.raster = raster;
        self.invalidate();
    }

    /// Force the next repaint to redraw every row.
    ///
    /// **A caller that changes which terminal it feeds this Painter MUST call
    /// this first.** `dirty` describes the NEW terminal's damage, and a
    /// terminal that has been sitting still reports nothing dirty — so a
    /// same-geometry switch (a tab or pane change, T3) would leave the
    /// previous occupant's pixels on every row the newcomer did not happen to
    /// touch. Nothing in the types ties a surface to a terminal, which is why
    /// this is stated rather than enforced (cold-review finding, 2026-09-21);
    /// T2 holds by construction, with one terminal for the process's life.
    pub fn invalidate(&mut self) {
        let mut frame = self.frame.lock().expect("frame lock");
        frame.surface.invalidate();
    }

    /// Rasterise `screen` into the shared surface, repainting only the rows
    /// `dirty` marks. Returns whether the frame changed.
    ///
    /// `screen` and `dirty` must come from consecutive `grid_snapshot` calls
    /// on the SAME terminal; see [`Painter::invalidate`].
    pub fn repaint(&mut self, screen: &Screen, dirty: &[bool]) -> bool {
        let mut frame = self.frame.lock().expect("frame lock");
        let before = frame.surface.grid();
        let bands = self.raster.render_into(screen, dirty, &mut frame.surface);
        if bands.is_empty() {
            // An empty band list usually means "nothing changed" — but a
            // screen with no paintable rows clears the surface and also
            // returns nothing, and a renderer told "no change" would then go
            // on presenting a texture whose source is gone (cold-review
            // finding, 2026-09-21). The geometry is what distinguishes them.
            if before != frame.surface.grid() {
                frame.generation += 1;
                frame.damage.clear();
                return true;
            }
            return false;
        }
        frame.damage.extend(bands);
        // Coalesced on the way IN, not only on the way out: the CPU arm never
        // drains bands (tiny-skia re-blits the whole handle), so without this
        // the list would grow for the life of the process. Merging bounds it
        // to at most one entry per two rows however long presentation stalls.
        frame.damage = coalesce(std::mem::take(&mut frame.damage));
        frame.generation += 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_term_core::config::Cursor;
    use cosmix_term_core::terminal::Cell;
    use std::time::Instant;

    fn band(y: u32, height: u32) -> DamageBand {
        DamageBand { y, height }
    }

    fn screen(cols: usize, rows: usize, fill: char) -> Screen {
        Screen {
            cols,
            rows,
            cursor: (0, 0),
            cursor_visible: false,
            cells: (0..cols * rows)
                .map(|_| Cell {
                    c: fill,
                    fg: [200, 200, 200],
                    bg: [0, 0, 0],
                    bold: false,
                })
                .collect(),
            updated: Instant::now(),
        }
    }

    fn painter() -> Painter {
        Painter::new(
            Raster::new(1.0, 13.0, Cursor::Underline)
                .expect("a monospace font; set TERM_SPIKE_FONT to point at one"),
        )
    }

    /// The whole point of the frontend, as an assertion: repeated repaints
    /// reuse one allocation, and a repaint with nothing dirty does nothing at
    /// all. A regression here is a return to `Image::new` per frame, which is
    /// invisible in behaviour and catastrophic in memory.
    #[test]
    fn repainting_reuses_one_buffer_and_a_clean_frame_costs_nothing() {
        let mut painter = painter();
        let shared = painter.frame();
        let grid = screen(20, 6, 'x');

        assert!(painter.repaint(&grid, &[]), "the first frame is owed");
        let (pointer, capacity, generation) = {
            let frame = shared.lock().unwrap();
            (
                frame.surface().rgba().as_ptr(),
                frame.surface().rgba().len(),
                frame.generation(),
            )
        };
        assert_eq!(generation, 1);

        let mut dirty = vec![false; 6];
        dirty[3] = true;
        assert!(painter.repaint(&grid, &dirty));
        {
            let frame = shared.lock().unwrap();
            assert_eq!(
                frame.surface().rgba().as_ptr(),
                pointer,
                "a repaint reallocated the grid buffer"
            );
            assert_eq!(frame.surface().rgba().len(), capacity);
            assert_eq!(frame.generation(), 2);
        }

        // Nothing dirty, no cursor: no write, no damage, no generation bump,
        // so a renderer woken for an unrelated reason uploads nothing.
        assert!(!painter.repaint(&grid, &[false; 6]));
        assert_eq!(shared.lock().unwrap().generation(), 2);
    }

    /// Cold-review finding (2026-09-21, both arms): two font sizes can round
    /// to the same integer cell — 13.0 px and 12.9 px are both 8x16 in
    /// DejaVuSansMono — so `render_into`'s cell-size check cannot see the
    /// swap. Only the swap site can.
    #[test]
    fn swapping_the_raster_repaints_even_when_the_cell_size_is_unchanged() {
        let mut painter = painter();
        let shared = painter.frame();
        let grid = screen(8, 4, 'M');
        let _ = painter.repaint(&grid, &[]);
        let cell = painter.cell();

        let other = Raster::new(1.0, 12.9, Cursor::Underline).expect("a monospace font");
        assert_eq!(
            (other.width, other.height),
            cell,
            "this test is only meaningful while the two sizes share a cell"
        );
        painter.replace_raster(other);
        // Nothing dirty, same geometry — and it must still repaint whole.
        assert!(painter.repaint(&grid, &[false; 4]));
        assert_eq!(
            shared.lock().unwrap().take_damage(),
            vec![band(0, 4 * cell.1)]
        );
    }

    /// The CPU arm never takes bands, so without coalescing on append the
    /// list would grow for the life of the process (cold-review finding).
    #[test]
    fn damage_stays_bounded_when_nobody_drains_it() {
        let mut painter = painter();
        let shared = painter.frame();
        let grid = screen(8, 4, 'M');
        let _ = painter.repaint(&grid, &[]);
        shared.lock().unwrap().clear_damage();
        for _ in 0..500 {
            let mut dirty = vec![false; 4];
            dirty[1] = true;
            assert!(painter.repaint(&grid, &dirty));
        }
        // Assert on the STORED list, not on `take_damage`'s output: that
        // coalesces on the way out, so it would report one band however many
        // are held, and the test would pass while the leak ran.
        assert_eq!(
            shared.lock().unwrap().damage.len(),
            1,
            "500 repaints of one row must be merged as they arrive, not held"
        );
    }

    /// A surface cleared to nothing is a change, and a renderer told "no
    /// change" would keep presenting pixels whose source is gone.
    #[test]
    fn clearing_the_surface_counts_as_a_change() {
        let mut painter = painter();
        let shared = painter.frame();
        assert!(painter.repaint(&screen(8, 4, 'M'), &[]));
        let generation = shared.lock().unwrap().generation();
        assert!(painter.repaint(&screen(0, 0, ' '), &[]));
        let frame = shared.lock().unwrap();
        assert!(frame.surface().is_empty());
        assert_eq!(frame.generation(), generation + 1);
        assert!(frame.damage.is_empty(), "there is nothing left to upload");
    }

    #[test]
    fn damage_accumulates_between_presents_and_clears_on_take() {
        let mut painter = painter();
        let shared = painter.frame();
        let grid = screen(20, 6, 'x');
        let cell_height = painter.cell().1;
        let _ = painter.repaint(&grid, &[]);
        shared.lock().unwrap().clear_damage();

        // Two rasters land before the renderer looks: both must survive, and
        // arrive as one band each because they are not adjacent.
        let mut first = vec![false; 6];
        first[0] = true;
        let mut second = vec![false; 6];
        second[4] = true;
        assert!(painter.repaint(&grid, &first));
        assert!(painter.repaint(&grid, &second));
        let mut frame = shared.lock().unwrap();
        assert_eq!(
            frame.take_damage(),
            vec![
                band(0, cell_height),
                band(4 * cell_height, cell_height)
            ]
        );
        assert_eq!(frame.take_damage(), vec![], "damage is consumed once");
    }

    #[test]
    fn coalescing_merges_touching_and_overlapping_bands_only() {
        assert_eq!(coalesce(vec![]), vec![]);
        assert_eq!(coalesce(vec![band(10, 5)]), vec![band(10, 5)]);
        // Touching (10..20 and 20..30) is one upload, not two.
        assert_eq!(
            coalesce(vec![band(20, 10), band(10, 10)]),
            vec![band(10, 20)]
        );
        // Overlapping, out of order, and fully contained.
        assert_eq!(
            coalesce(vec![band(0, 30), band(10, 5), band(25, 10)]),
            vec![band(0, 35)]
        );
        // A real gap survives: merging these would upload the rows between
        // them, which is exactly the whole-screen cost being avoided.
        assert_eq!(
            coalesce(vec![band(0, 10), band(40, 10)]),
            vec![band(0, 10), band(40, 10)]
        );
    }
}
