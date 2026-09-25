//! The grid images, and the only thing the renderer and the VT loop share.
//!
//! A [`Frame`] is created per visible pane and lives while that pane is on
//! screen. The VT loop rasterises into it in place and appends the damaged
//! bands; the renderer consumes them. The wgpu arm keeps one Vec-backed
//! surface; tiny-skia shares each native Bytes-backed band with its generation,
//! reclaiming it for painting or copying if iced still holds it. Reusing
//! storage matters: the Bevy terminal's `Image::new`-per-damaged-frame
//! is where 320 MB of its 344 MB of mapped GEM went
//! (`_journal/2026-09-20-term-vs-foot-memory-anatomy.md`).

#[cfg(all(feature = "tiny-skia", not(feature = "wgpu")))]
use crate::cpu_grid::Surface;
use cosmix_term_core::config::Cursor;
use cosmix_term_core::font::FontSize;
#[cfg(not(all(feature = "tiny-skia", not(feature = "wgpu"))))]
use cosmix_term_core::raster::Surface;
use cosmix_term_core::raster::{DamageBand, Raster};
use cosmix_term_core::terminal::Screen;
use std::collections::HashMap;
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
    /// native-grid generation on it; tests use it to tell a real repaint from a
    /// no-op.
    generation: u64,
}

impl Frame {
    #[cfg(all(feature = "tiny-skia", not(feature = "wgpu")))]
    pub fn cpu_surface_mut(&mut self) -> &mut Surface {
        &mut self.surface
    }

    pub fn surface(&self) -> &Surface {
        &self.surface
    }

    // Read by the CPU arm's native generation cache and tests; the wgpu arm keys
    // its uploads on damage bands instead, so it never asks.
    #[cfg_attr(feature = "wgpu", allow(dead_code))]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Damage since the last call, coalesced, and cleared.
    // The CPU arm tracks damage in its per-band handles; only the wgpu arm
    // and the tests consume this separate upload list.
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

/// Merge matching horizontal spans vertically; never lose x/width when
/// accumulating paints between uploads. Repeated ranges remain bounded.
fn coalesce(mut bands: Vec<DamageBand>) -> Vec<DamageBand> {
    if bands.len() < 2 {
        return bands;
    }
    bands.sort_unstable_by_key(|band| (band.x, band.width, band.y));
    let mut merged: Vec<DamageBand> = Vec::with_capacity(bands.len());
    for band in bands {
        match merged.last_mut() {
            Some(last)
                if band.x == last.x
                    && band.width == last.width
                    && band.y <= last.y + last.height =>
            {
                let end = (band.y + band.height).max(last.y + last.height);
                last.height = end - last.y;
            }
            _ => merged.push(band),
        }
    }
    merged
}

/// The app-side half: ONE glyph cache for every pane, the font size it was
/// built at, and a frame per visible pane.
///
/// The [`Raster`] deliberately does NOT live inside a shared `Frame`. The
/// renderer needs the pixels and nothing else, and a swash `ScaleContext`
/// behind the same lock the GPU thread takes would make every glyph miss
/// contend with every upload. It is shared across panes because every pane
/// draws the same font at the same size — a raster per pane would parse the
/// font file and grow a glyph cache per split.
///
/// **Frames are keyed by pane id, and that is what keeps a surface honest.**
/// `render_into` repaints only the rows the snapshot marks dirty, so a surface
/// fed a different terminal than last time would keep the previous occupant's
/// pixels on every row the newcomer left alone (cold-review finding,
/// 2026-09-21). A frame here only ever receives its own pane's snapshots, and
/// the core never reuses a pane id, so that cannot happen. A pane that leaves
/// the screen (its tab is hidden) loses its frame; coming back it gets a fresh
/// one, whose empty surface forces a full first repaint.
pub struct Painter {
    raster: Raster,
    font: FontSize,
    frames: HashMap<u64, Arc<Mutex<Frame>>>,
}

impl Painter {
    pub fn new(scale: f32, font: FontSize, cursor: Cursor) -> Result<Self, String> {
        Ok(Self {
            raster: Raster::new(scale, font.current(), cursor)?,
            font,
            frames: HashMap::new(),
        })
    }

    /// The frame pane `id` is drawn into, created on first use.
    pub fn frame(&mut self, id: u64) -> Arc<Mutex<Frame>> {
        self.frames.entry(id).or_default().clone()
    }

    /// The frame for a pane already on screen, without creating one — for
    /// `view`, which must not mutate.
    pub fn existing(&self, id: u64) -> Option<Arc<Mutex<Frame>>> {
        self.frames.get(&id).cloned()
    }

    /// Drop the frames of panes no longer on screen, so a hidden tab holds no
    /// grid-sized buffers (and the GPU arm's `trim` frees their textures).
    pub fn retain(&mut self, visible: &[u64]) {
        self.frames.retain(|id, _| visible.contains(id));
    }

    /// Physical cell size, for turning a pane into a column count.
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

    // Read by the tests; nothing in the app needs the size back until a
    // font verb exists (T7).
    #[cfg(test)]
    pub fn font(&self) -> FontSize {
        self.font
    }

    /// Rebuild for a new device scale, at the CURRENT font size — not the
    /// configured one, or moving the window to another output would silently
    /// undo the user's zoom. Ok(false) when the scale did not move.
    pub fn set_scale(&mut self, scale: f32) -> Result<bool, String> {
        if (scale - self.raster.scale).abs() < 0.01 {
            return Ok(false);
        }
        self.replace_raster(self.raster.resized(scale, self.font.current())?);
        Ok(true)
    }

    /// Apply a zoom (`FontSize::increase` and friends) and rebuild the raster
    /// if it changed the size. The new size is committed only once a raster
    /// for it exists: a failed rebuild leaves both the size and the glyphs as
    /// they were, rather than a size nobody is drawing at.
    pub fn zoom(&mut self, change: impl FnOnce(&mut FontSize) -> bool) -> Result<bool, String> {
        let mut font = self.font;
        if !change(&mut font) {
            return Ok(false);
        }
        let raster = self.raster.resized(self.raster.scale, font.current())?;
        self.font = font;
        self.replace_raster(raster);
        Ok(true)
    }

    /// Swap in a raster built for a new scale or font size, and force a full
    /// repaint of every pane.
    ///
    /// The invalidation is NOT belt-and-braces. `render_into` decides "is a
    /// full repaint owed?" from the surface's recorded cell size, and two
    /// different font sizes can round to the same integer cell — 13.0 px and
    /// 12.9 px both give an 8x16 DejaVuSansMono cell, with visibly different
    /// glyphs inside it (cold-review finding, 2026-09-21, reproduced). The
    /// swap invalidates every pane immediately. Core also records a raster
    /// identity token, so direct core callers cannot miss a same-metrics swap.
    pub fn replace_raster(&mut self, raster: Raster) {
        self.raster = raster;
        for frame in self.frames.values() {
            frame.lock().expect("frame lock").surface.invalidate();
        }
    }

    /// Rasterise `screen` into pane `id`'s surface, repainting only the rows
    /// `dirty` marks. Returns whether the frame changed.
    ///
    /// `screen` and `dirty` must come from consecutive `grid_snapshot` calls
    /// on pane `id`'s terminal; see the type-level note.
    pub fn repaint(&mut self, id: u64, screen: &Screen, dirty: &[bool]) -> bool {
        let frame = self.frame(id);
        let mut frame = frame.lock().expect("frame lock");
        // Whether the surface HAS pixels, not what shape they are in. The
        // first cut compared `grid()`, which `invalidate` also resets to
        // (0, 0) — so `repaint(nonempty) -> invalidate -> repaint(empty)`
        // compared (0,0) against (0,0), reported no change, and left the CPU
        // arm presenting a stale image (re-review residual, 2026-09-21). A
        // geometry change that is not a clear always produces bands, so
        // emptiness is the only no-band change there is.
        let had_pixels = !frame.surface.is_empty();
        // Scoped, because the bands borrow the surface: copying them into
        // `damage` here ends that borrow, and `frame` is whole again below.
        // `damage` and `surface` are disjoint fields, so both are reachable.
        let painted = {
            let Frame {
                surface, damage, ..
            } = &mut *frame;
            #[cfg(not(all(feature = "tiny-skia", not(feature = "wgpu"))))]
            let bands = self.raster.render_into(screen, dirty, surface);
            #[cfg(all(feature = "tiny-skia", not(feature = "wgpu")))]
            let bands = surface.paint(&mut self.raster, screen, dirty);
            damage.extend_from_slice(bands);
            !bands.is_empty()
        };
        if !painted {
            // An empty band list usually means "nothing changed" — but a
            // screen with no paintable rows clears the surface and also
            // returns nothing, and a renderer told "no change" would go on
            // presenting a texture whose source is gone.
            if had_pixels && frame.surface.is_empty() {
                frame.generation += 1;
                frame.damage.clear();
                return true;
            }
            // paint may have released the CPU handle before finding no
            // damage. The caller skips refresh on false, so restore it here
            // at the unchanged generation rather than showing a placeholder.
            #[cfg(all(feature = "tiny-skia", not(feature = "wgpu")))]
            {
                let generation = frame.generation;
                frame.surface.cache_handle(generation);
            }
            return false;
        }
        // Coalesced on the way IN, not only on the way out: several paints
        // can arrive before refresh drains damage. Merging bounds the list
        // by the grid's distinct cell ranges, however long presentation stalls.
        frame.damage = coalesce(std::mem::take(&mut frame.damage));
        frame.generation += 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_term_core::terminal::Cell;
    use std::time::Instant;

    const PANE: u64 = 1;

    fn band(y: u32, height: u32) -> DamageBand {
        DamageBand {
            x: 0,
            width: 1,
            y,
            height,
        }
    }

    fn screen(cols: usize, rows: usize, fill: char) -> Screen {
        Screen {
            cols,
            rows,
            cursor: (0, 0),
            cursor_visible: false,
            display_offset: 0,
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
        painter_at(13.0)
    }

    fn painter_at(px: f32) -> Painter {
        Painter::new(1.0, FontSize::new(px), Cursor::Underline)
            .expect("a monospace font; set TERM_SPIKE_FONT to point at one")
    }

    /// The whole point of the frontend, as an assertion: repeated repaints
    /// reuse one allocation, and a repaint with nothing dirty does nothing at
    /// all. A regression here is a return to `Image::new` per frame, which is
    /// invisible in behaviour and catastrophic in memory.
    #[test]
    fn repainting_reuses_one_buffer_and_a_clean_frame_costs_nothing() {
        let mut painter = painter();
        let shared = painter.frame(PANE);
        let mut grid = screen(20, 6, 'x');

        assert!(painter.repaint(PANE, &grid, &[]), "the first frame is owed");
        let (pointer, capacity, generation) = {
            let frame = shared.lock().unwrap();
            (
                {
                    #[cfg(all(feature = "tiny-skia", not(feature = "wgpu")))]
                    {
                        frame.surface().allocation()
                    }
                    #[cfg(not(all(feature = "tiny-skia", not(feature = "wgpu"))))]
                    {
                        frame.surface().rgba().as_ptr()
                    }
                },
                frame.surface().rgba().len(),
                frame.generation(),
            )
        };
        assert_eq!(generation, 1);

        let mut dirty = vec![false; 6];
        dirty[3] = true;
        grid.cells[3 * 20].c = 'M';
        assert!(painter.repaint(PANE, &grid, &dirty));
        {
            let frame = shared.lock().unwrap();
            assert_eq!(
                {
                    #[cfg(all(feature = "tiny-skia", not(feature = "wgpu")))]
                    {
                        frame.surface().allocation()
                    }
                    #[cfg(not(all(feature = "tiny-skia", not(feature = "wgpu"))))]
                    {
                        frame.surface().rgba().as_ptr()
                    }
                },
                pointer,
                "a repaint reallocated the grid buffer"
            );
            assert_eq!(frame.surface().rgba().len(), capacity);
            assert_eq!(frame.generation(), 2);
        }

        // Nothing dirty, no cursor: no write, no damage, no generation bump,
        // so a renderer woken for an unrelated reason uploads nothing.
        assert!(!painter.repaint(PANE, &grid, &[false; 6]));
        assert_eq!(shared.lock().unwrap().generation(), 2);
    }

    /// Cold-review finding (2026-09-21, both arms): two font sizes can round
    /// to the same integer cell — 13.0 px and 12.9 px are both 8x16 in
    /// DejaVuSansMono — so `render_into`'s cell-size check cannot see the
    /// swap. Only the swap site can.
    #[test]
    fn swapping_the_raster_repaints_even_when_the_cell_size_is_unchanged() {
        let mut painter = painter();
        let shared = painter.frame(PANE);
        let grid = screen(8, 4, 'M');
        let _ = painter.repaint(PANE, &grid, &[]);
        let cell = painter.cell();

        let other = Raster::new(1.0, 12.9, Cursor::Underline).expect("a monospace font");
        assert_eq!(
            (other.width, other.height),
            cell,
            "this test is only meaningful while the two sizes share a cell"
        );
        painter.replace_raster(other);
        // Nothing dirty, same geometry — and it must still repaint whole.
        assert!(painter.repaint(PANE, &grid, &[false; 4]));
        assert_eq!(
            shared.lock().unwrap().take_damage(),
            vec![DamageBand {
                width: 8 * cell.0,
                ..band(0, 4 * cell.1)
            }]
        );
    }

    /// The CPU arm never takes bands, so without coalescing on append the
    /// list would grow for the life of the process (cold-review finding).
    #[test]
    fn damage_stays_bounded_when_nobody_drains_it() {
        let mut painter = painter();
        let shared = painter.frame(PANE);
        let mut grid = screen(8, 4, 'M');
        let _ = painter.repaint(PANE, &grid, &[]);
        shared.lock().unwrap().clear_damage();
        for n in 0..500 {
            let mut dirty = vec![false; 4];
            dirty[1] = true;
            grid.cells[8].bg[0] = (n % 255 + 1) as u8;
            assert!(painter.repaint(PANE, &grid, &dirty));
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
        let shared = painter.frame(PANE);
        assert!(painter.repaint(PANE, &screen(8, 4, 'M'), &[]));
        let generation = shared.lock().unwrap().generation();
        assert!(painter.repaint(PANE, &screen(0, 0, ' '), &[]));
        {
            let frame = shared.lock().unwrap();
            assert!(frame.surface().is_empty());
            assert_eq!(frame.generation(), generation + 1);
            assert!(frame.damage.is_empty(), "there is nothing left to upload");
        }

        // Re-review residual (2026-09-21): the first cut compared the
        // surface's GRID, which `invalidate` also resets to (0, 0) — so an
        // invalidate between the two repaints made the clear compare (0,0)
        // against (0,0) and report no change at all.
        assert!(painter.repaint(PANE, &screen(8, 4, 'M'), &[]));
        let generation = shared.lock().unwrap().generation();
        shared.lock().unwrap().surface.invalidate();
        assert!(painter.repaint(PANE, &screen(0, 0, ' '), &[]));
        let frame = shared.lock().unwrap();
        assert!(frame.surface().is_empty());
        assert_eq!(frame.generation(), generation + 1);
    }

    #[test]
    fn damage_accumulates_between_presents_and_clears_on_take() {
        let mut painter = painter();
        let shared = painter.frame(PANE);
        let mut grid = screen(20, 6, 'x');
        let cell_height = painter.cell().1;
        let _ = painter.repaint(PANE, &grid, &[]);
        shared.lock().unwrap().clear_damage();

        // Two rasters land before the renderer looks: both must survive, and
        // arrive as one band each because they are not adjacent.
        let mut first = vec![false; 6];
        first[0] = true;
        let mut second = vec![false; 6];
        second[4] = true;
        grid.cells[0].c = 'M';
        assert!(painter.repaint(PANE, &grid, &first));
        grid.cells[4 * 20].c = 'M';
        assert!(painter.repaint(PANE, &grid, &second));
        let mut frame = shared.lock().unwrap();
        assert_eq!(
            frame.take_damage(),
            vec![
                DamageBand {
                    width: painter.cell().0,
                    ..band(0, cell_height)
                },
                DamageBand {
                    width: painter.cell().0,
                    ..band(4 * cell_height, cell_height)
                }
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
        let left = DamageBand {
            x: 4,
            width: 8,
            y: 10,
            height: 20,
        };
        let right = DamageBand { x: 80, ..left };
        assert_eq!(coalesce(vec![right, left, left]), vec![left, right]);
    }

    /// T3: two panes on screen must each own their pixels. A single shared
    /// surface would make pane B's repaint overwrite pane A's grid.
    #[test]
    fn each_pane_paints_into_its_own_frame() {
        let mut painter = painter();
        let left = painter.frame(1);
        let right = painter.frame(2);
        assert!(!Arc::ptr_eq(&left, &right));

        assert!(painter.repaint(1, &screen(8, 4, 'L'), &[]));
        let left_pixels = left.lock().unwrap().surface().rgba().to_vec();
        assert!(painter.repaint(2, &screen(8, 4, 'R'), &[]));
        assert_eq!(
            left.lock().unwrap().surface().rgba(),
            left_pixels.as_slice(),
            "painting pane 2 changed pane 1's pixels"
        );
        assert_ne!(
            right.lock().unwrap().surface().rgba(),
            left_pixels.as_slice(),
            "different glyphs must give different pixels, or this test proves nothing"
        );
    }

    /// T3: a hidden tab's panes hold no buffers, and a pane that comes back
    /// repaints in full — its terminal reports only rows dirtied SINCE the
    /// last snapshot, which says nothing about a surface it never drew into.
    #[test]
    fn hidden_panes_lose_their_frames_and_come_back_whole() {
        let mut painter = painter();
        let grid = screen(8, 4, 'x');
        let _ = painter.repaint(1, &grid, &[]);
        let _ = painter.repaint(2, &grid, &[]);

        painter.retain(&[2]);
        assert!(
            painter.existing(1).is_none(),
            "the hidden pane kept its frame"
        );
        assert!(painter.existing(2).is_some());

        // Back on screen with nothing dirty: still a whole repaint.
        assert!(painter.repaint(1, &grid, &[false; 4]));
        let cell_height = painter.cell().1;
        assert_eq!(
            painter.frame(1).lock().unwrap().take_damage(),
            vec![DamageBand {
                width: 8 * painter.cell().0,
                ..band(0, 4 * cell_height)
            }]
        );
    }

    /// T4: a zoom re-rasterises EVERY visible pane, not only the focused one,
    /// because they share the glyph cache it replaced.
    #[test]
    fn zooming_rebuilds_the_raster_and_repaints_every_pane() {
        let mut painter = painter();
        let grid = screen(8, 4, 'x');
        let _ = painter.repaint(1, &grid, &[]);
        let _ = painter.repaint(2, &grid, &[]);
        for id in [1, 2] {
            painter.frame(id).lock().unwrap().clear_damage();
        }
        let before = painter.cell();

        assert!(painter.zoom(FontSize::increase).unwrap());
        for _ in 0..5 {
            painter.zoom(FontSize::increase).unwrap();
        }
        assert!(
            painter.cell().1 > before.1,
            "six steps up must grow the cell"
        );
        assert_eq!(
            painter.cell(),
            painter_at(painter.font().current()).cell(),
            "the raster must be built at the zoomed size"
        );
        for id in [1, 2] {
            assert!(
                painter.repaint(id, &grid, &[false; 4]),
                "pane {id} kept glyphs from the old size"
            );
        }

        assert!(painter.zoom(FontSize::reset).unwrap());
        assert_eq!(painter.cell(), before);
        assert!(
            !painter.zoom(FontSize::reset).unwrap(),
            "a no-op zoom rebuilds nothing"
        );
    }

    /// T4: the zoom survives a scale change. Rebuilding from the configured
    /// size would un-zoom the terminal every time it moved between outputs.
    #[test]
    fn a_rescale_keeps_the_zoomed_size() {
        let mut painter = painter();
        painter.zoom(|font| font.step_by(6)).unwrap();
        let zoomed = painter.font().current();
        assert_ne!(zoomed, 13.0);

        assert!(painter.set_scale(2.0).unwrap());
        assert_eq!(painter.font().current(), zoomed);
        let expected = Raster::new(2.0, zoomed, Cursor::Underline).unwrap();
        assert_eq!(painter.cell(), (expected.width, expected.height));
        assert!(!painter.set_scale(2.0).unwrap(), "same scale, no rebuild");
    }
}
