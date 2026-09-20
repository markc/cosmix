use crate::terminal::Screen;
use std::{collections::HashMap, path::PathBuf};
use swash::{
    FontRef,
    scale::{Render, ScaleContext, Source, image::Image},
    zeno::Format,
};

/// A contiguous run of damaged device-pixel rows in a [`Surface`].
///
/// `y` and `height` are in the surface's own physical pixels, which is what
/// both `wgpu::Queue::write_texture` and a `wl_shm` damage rectangle want.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DamageBand {
    pub y: u32,
    pub height: u32,
}

impl DamageBand {
    /// Byte range of this band in a surface whose rows are `stride` bytes.
    pub fn byte_range(&self, stride: usize) -> std::ops::Range<usize> {
        self.y as usize * stride..(self.y as usize + self.height as usize) * stride
    }
}

/// A persistent RGBA8 surface that [`Raster::render_into`] mutates in place.
///
/// The point of this type is what it is *not*: a `Vec<u8>` returned by value
/// per frame. `Raster::render` allocates one whole grid image on every call
/// (~12 MB at 2.5x scale), which is what made the Bevy frontend build a brand
/// new `Image` per damaged frame — three GEM objects holding 320 MB of its
/// 344 MB of mapped device memory (`_journal/2026-09-20-term-vs-foot-memory-
/// anatomy.md`). A frontend owns one of these per pane for the pane's life and
/// re-rasterises only the rows the grid reports dirty.
///
/// It carries the geometry it was last drawn at, so it — not the caller —
/// decides when a full repaint is owed, and the cursor cell it last drew, so
/// the old cursor is always erased even across a frame the caller skipped.
#[derive(Default)]
pub struct Surface {
    rgba: Vec<u8>,
    width: u32,
    height: u32,
    cols: usize,
    rows: usize,
    cell: (u32, u32),
    cursor: Option<(usize, usize)>,
}

impl Surface {
    /// Premultiplication-free RGBA8, row-major, `width * height * 4` bytes.
    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }
    /// Physical pixels.
    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
    pub fn stride(&self) -> usize {
        self.width as usize * 4
    }
    /// Cells, as of the last `render_into`.
    pub fn grid(&self) -> (usize, usize) {
        (self.cols, self.rows)
    }
    pub fn is_empty(&self) -> bool {
        self.rgba.is_empty()
    }
    /// Forget the drawn content without freeing the allocation, so the next
    /// `render_into` repaints every row. For a frontend that lost its GPU
    /// texture (surface recreation) and must re-upload the whole grid.
    pub fn invalidate(&mut self) {
        self.cols = 0;
        self.rows = 0;
        self.cell = (0, 0);
        self.cursor = None;
    }
}

/// Runs of `true` in `rows`, in device-pixel coordinates.
fn bands(rows: &[bool], cell_height: u32) -> Vec<DamageBand> {
    let mut bands: Vec<DamageBand> = Vec::new();
    let mut start: Option<usize> = None;
    for (index, dirty) in rows.iter().chain(std::iter::once(&false)).enumerate() {
        match (dirty, start) {
            (true, None) => start = Some(index),
            (false, Some(first)) => {
                bands.push(DamageBand {
                    y: first as u32 * cell_height,
                    height: (index - first) as u32 * cell_height,
                });
                start = None;
            }
            _ => {}
        }
    }
    bands
}

pub struct Raster {
    cursor: crate::config::Cursor,
    data: Vec<u8>,
    context: ScaleContext,
    cache: HashMap<(char, bool, [u8; 3]), Option<Image>>,
    /// Cell dimensions in PHYSICAL device pixels: the texture is rasterised at
    /// the display's true resolution so the compositor never has to upscale
    /// (which, on a fractional-scale HiDPI output, is what makes text blurry).
    pub width: u32,
    pub height: u32,
    /// Device-pixels-per-logical-pixel this Raster was built for. The on-screen
    /// node is sized at `width/scale` logical px so the texture maps 1:1 to
    /// physical pixels. Rebuild the Raster when the window's scale changes.
    pub scale: f32,
    px: f32,
    baseline: i32,
}
impl Raster {
    pub fn new(scale: f32, logical_px: f32, cursor: crate::config::Cursor) -> Result<Self, String> {
        let scale = scale.clamp(0.5, 8.0);
        // Startup resolves logical size once; scale makes it physical for HiDPI.
        let px = logical_px * scale;
        let path = if let Some(path) = std::env::var_os("TERM_SPIKE_FONT") {
            PathBuf::from(path)
        } else {
            [
                "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
                "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
                "/usr/share/fonts/liberation/LiberationMono-Regular.ttf",
                "/usr/share/fonts/truetype/liberation2/LiberationMono-Regular.ttf",
                "/usr/share/fonts/noto/NotoSansMono-Regular.ttf",
                "/usr/share/fonts/truetype/noto/NotoSansMono-Regular.ttf",
                "/usr/share/fonts/TTF/JetBrainsMono-Regular.ttf",
            ]
            .into_iter()
            .map(PathBuf::from)
            .find(|p| p.is_file())
            .ok_or("No monospace font found; set TERM_SPIKE_FONT=/path/to/font.ttf")?
        };
        let data = std::fs::read(&path)
            .map_err(|e| format!("font {}: {e}; set TERM_SPIKE_FONT", path.display()))?;
        let font = FontRef::from_index(&data, 0)
            .ok_or("Invalid font; set TERM_SPIKE_FONT to a TTF/OTF font")?;
        let metrics = font.metrics(&[]).scale(px);
        let advance = font
            .glyph_metrics(&[])
            .scale(px)
            .advance_width(font.charmap().map('M'));
        let width = advance.ceil().max(1.0) as u32;
        let height = (metrics.ascent + metrics.descent.abs() + metrics.leading)
            .ceil()
            .max(1.0) as u32;
        let baseline = metrics.ascent.ceil() as i32;
        // Physical-pixel cells scale with the display; allow generous HiDPI room.
        if width > 512 || height > 1024 {
            return Err("font metrics exceed cell limits".into());
        }
        eprintln!(
            "DIAGNOSTIC font={} scale={scale} cell={width}x{height} (physical px); bold=regular+brighter colour",
            path.display()
        );
        Ok(Self {
            cursor,
            data,
            context: ScaleContext::new(),
            cache: HashMap::new(),
            width,
            height,
            scale,
            px,
            baseline,
        })
    }
    /// Logical (unscaled) cell dimensions, for sizing the on-screen node.
    pub fn logical_width(&self) -> f32 {
        self.width as f32 / self.scale
    }
    pub fn logical_height(&self) -> f32 {
        self.height as f32 / self.scale
    }
    /// Whole-grid render into a fresh buffer.
    ///
    /// Kept for the Bevy frontend, which uploads a whole image per frame.
    /// Every call allocates `cols * rows * cell * 4` bytes; prefer
    /// [`Raster::render_into`], which reuses one [`Surface`] and repaints only
    /// the rows the grid reports dirty.
    pub fn render(&mut self, screen: &Screen) -> Vec<u8> {
        let mut surface = Surface::default();
        let _ = self.render_into(screen, &[], &mut surface);
        surface.rgba
    }

    /// Rasterise `screen` into `surface` **in place**, repainting only the
    /// rows `dirty` marks, and return the damaged device-pixel bands.
    ///
    /// `dirty` is `GridSnapshot::dirty_rows`: one flag per visible row. A
    /// length that does not match `screen.rows` is treated as "everything",
    /// so a caller that cannot supply damage (`&[]`) still gets a correct
    /// frame — only a slower one.
    ///
    /// A `screen` whose `cells` are shorter than `cols * rows` is painted as
    /// the whole rows it does contain, and the surface is sized to those. The
    /// returned bands therefore never claim a row this call did not write.
    ///
    /// The returned bands are exactly the regions of `surface.rgba()` this
    /// call wrote. An empty result means the surface already holds the frame
    /// and the caller owes the GPU (or the compositor) nothing at all, which
    /// is the idle case a terminal spends almost all of its life in.
    ///
    /// Two rows are always repainted beyond `dirty`: the one the cursor is on
    /// now, and the one this surface last drew a cursor on. The cursor overlay
    /// belongs to this function, not to the VT, so nothing else can be relied
    /// on to erase it — in particular when the caller turns the cursor off
    /// (an unfocused pane) without the grid changing at all.
    pub fn render_into(
        &mut self,
        screen: &Screen,
        dirty: &[bool],
        surface: &mut Surface,
    ) -> Vec<DamageBand> {
        // `Screen`'s fields are public, so a cell array shorter than
        // `cols * rows` is constructible even though `Terminal::capture` never
        // produces one. Paint only the whole rows that actually exist: the
        // alternative is a returned band claiming a row was repainted while
        // the loop skipped it, which is a lie a renderer cannot detect and
        // which leaves the old pixels — including an old cursor — on screen.
        let rows = screen.rows.min(screen.cells.len() / screen.cols.max(1));
        if screen.cols == 0 || rows == 0 {
            *surface = Surface::default();
            return Vec::new();
        }
        let cell = (self.width, self.height);
        let width = screen.cols * self.width as usize;
        let height = rows * self.height as usize;
        let bytes = width * height * 4;
        // The surface's own record decides, never the caller's: a Raster
        // rebuilt at a new scale changes `cell` while cols/rows stay put, and
        // that must still force a full repaint.
        let full = surface.cols != screen.cols
            || surface.rows != rows
            || surface.cell != cell
            || surface.rgba.len() != bytes;
        if full {
            surface.rgba.clear();
            surface.rgba.resize(bytes, 0);
            surface.cols = screen.cols;
            surface.rows = rows;
            surface.cell = cell;
            surface.width = width as u32;
            surface.height = height as u32;
            surface.cursor = None;
        }
        let mut dirty_rows = vec![full; rows];
        if !full {
            // `dirty` is indexed against the VT's row count; a short cell
            // array shrinks the surface but not the snapshot, so the slice is
            // only trustworthy when both agree.
            if dirty.len() == screen.rows && screen.rows == rows {
                dirty_rows.copy_from_slice(dirty);
            } else {
                dirty_rows.fill(true);
            }
            if let Some((_, previous)) = surface.cursor
                && previous < rows
            {
                dirty_rows[previous] = true;
            }
            if screen.cursor_visible && screen.cursor.1 < rows {
                dirty_rows[screen.cursor.1] = true;
            }
        }
        let rgba = &mut surface.rgba;
        for (i, cell) in screen.cells.iter().take(screen.cols * rows).enumerate() {
            if !dirty_rows[i / screen.cols] {
                continue;
            }
            let x = (i % screen.cols) as i32 * self.width as i32;
            let y = (i / screen.cols) as i32 * self.height as i32;
            for cy in 0..self.height as usize {
                for cx in 0..self.width as usize {
                    let offset = ((y as usize + cy) * width + x as usize + cx) * 4;
                    rgba[offset..offset + 4]
                        .copy_from_slice(&[cell.bg[0], cell.bg[1], cell.bg[2], 255]);
                }
            }
            if cell.c == ' ' || cell.c == '\0' {
                continue;
            }
            let key = (cell.c, cell.bold, cell.fg);
            if !self.cache.contains_key(&key) {
                if self.cache.len() >= 4096 {
                    self.cache.clear();
                }
                let font = FontRef::from_index(&self.data, 0).unwrap();
                let mut scaler = self.context.builder(font).size(self.px).hint(true).build();
                let glyph = Render::new(&[Source::Outline])
                    .format(Format::Alpha)
                    .render(&mut scaler, font.charmap().map(cell.c));
                self.cache.insert(key, glyph);
            }
            if let Some(glyph) = &self.cache[&key] {
                let p = glyph.placement;
                for gy in 0..p.height as i32 {
                    for gx in 0..p.width as i32 {
                        let dx = x + p.left + gx;
                        let dy = y + self.baseline - p.top + gy;
                        // Strict cell clipping: shaping, wide and combining glyph layout
                        // are product work; never scribble into adjacent cells.
                        if dx < x
                            || dx >= x + self.width as i32
                            || dy < y
                            || dy >= y + self.height as i32
                        {
                            continue;
                        }
                        let alpha = glyph.data[(gy as u32 * p.width + gx as u32) as usize] as u32;
                        let offset = (dy as usize * width + dx as usize) * 4;
                        for channel in 0..3 {
                            rgba[offset + channel] = ((cell.fg[channel] as u32 * alpha
                                + rgba[offset + channel] as u32 * (255 - alpha))
                                / 255) as u8;
                        }
                    }
                }
            }
        }
        // Steady cursor; invert a block so its glyph remains readable.
        let (cx, cy) = screen.cursor;
        let drawn = screen.cursor_visible && cx < screen.cols && cy < rows;
        if drawn {
            let bottom = (cy + 1) * self.height as usize;
            let top = match self.cursor {
                crate::config::Cursor::Block => cy * self.height as usize,
                crate::config::Cursor::Underline => bottom - 1,
            };
            for y in top..bottom {
                for x in cx * self.width as usize..(cx + 1) * self.width as usize {
                    let offset = (y * width + x) * 4;
                    match self.cursor {
                        crate::config::Cursor::Block => {
                            for channel in &mut rgba[offset..offset + 3] {
                                *channel = 255 - *channel;
                            }
                        }
                        crate::config::Cursor::Underline => {
                            rgba[offset..offset + 4].copy_from_slice(&[220, 220, 220, 255]);
                        }
                    }
                }
            }
        }
        surface.cursor = drawn.then_some(screen.cursor);
        bands(&dirty_rows, self.height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Cursor;
    use crate::terminal::Cell;
    use std::time::Instant;

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

    /// Fails loudly rather than skipping: a raster test that quietly passes on
    /// a machine with no monospace font is worse than no test at all.
    fn raster() -> Raster {
        Raster::new(1.0, 13.0, Cursor::Underline)
            .expect("a monospace font; set TERM_SPIKE_FONT to point at one")
    }

    #[test]
    fn runs_of_dirty_rows_coalesce_into_bands() {
        assert_eq!(bands(&[false, false], 10), vec![]);
        assert_eq!(
            bands(&[true, true, false, true], 10),
            vec![
                DamageBand { y: 0, height: 20 },
                DamageBand { y: 30, height: 10 },
            ]
        );
        // A run that reaches the last row must still be closed.
        assert_eq!(
            bands(&[false, true, true], 4),
            vec![DamageBand { y: 4, height: 8 }]
        );
        assert_eq!(
            DamageBand { y: 4, height: 8 }.byte_range(40),
            160..480
        );
    }

    #[test]
    fn a_clean_row_is_not_rewritten_and_reports_no_damage() {
        let mut raster = raster();
        let mut surface = Surface::default();
        let mut grid = screen(4, 3, 'x');
        let first = raster.render_into(&grid, &[], &mut surface);
        assert_eq!(
            first,
            vec![DamageBand {
                y: 0,
                height: 3 * raster.height
            }],
            "the first render owes the whole surface"
        );

        // Poison every row, then repaint only the middle one. Anything the
        // call touches loses its poison; anything it skips keeps it.
        surface.rgba.fill(0x5a);
        let stride = surface.stride();
        let second = raster.render_into(&grid, &[false, true, false], &mut surface);
        assert_eq!(
            second,
            vec![DamageBand {
                y: raster.height,
                height: raster.height
            }]
        );
        let row = |r: u32| {
            let band = DamageBand {
                y: r * raster.height,
                height: raster.height,
            };
            surface.rgba[band.byte_range(stride)].to_vec()
        };
        assert!(row(0).iter().all(|b| *b == 0x5a), "row 0 was rewritten");
        assert!(row(2).iter().all(|b| *b == 0x5a), "row 2 was rewritten");
        assert!(row(1).iter().any(|b| *b != 0x5a), "row 1 was NOT rewritten");

        // And the instrument moves the other way: an all-dirty pass clears
        // every poisoned row, so the assertions above are not vacuous.
        surface.rgba.fill(0x5a);
        let third = raster.render_into(&grid, &[true, true, true], &mut surface);
        assert_eq!(third.len(), 1);
        assert!(!surface.rgba.contains(&0x5a));

        // Nothing dirty, no cursor: no work and no damage at all.
        assert_eq!(
            raster.render_into(&grid, &[false, false, false], &mut surface),
            vec![]
        );

        // A resize re-owes the whole surface even though `dirty` says nothing.
        grid = screen(4, 4, 'x');
        let resized = raster.render_into(&grid, &[false; 4], &mut surface);
        assert_eq!(
            resized,
            vec![DamageBand {
                y: 0,
                height: 4 * raster.height
            }]
        );
        assert_eq!(surface.grid(), (4, 4));
        assert_eq!(surface.rgba().len(), surface.stride() * surface.height() as usize);
    }

    #[test]
    fn the_cursor_damages_the_row_it_left_as_well_as_the_one_it_entered() {
        let mut raster = raster();
        let mut surface = Surface::default();
        let mut grid = screen(4, 3, ' ');
        grid.cursor_visible = true;
        grid.cursor = (0, 0);
        let _ = raster.render_into(&grid, &[], &mut surface);

        // The VT reports nothing dirty, but the cursor moved: both rows must
        // be repainted or the old cursor is left behind.
        grid.cursor = (0, 2);
        let moved = raster.render_into(&grid, &[false; 3], &mut surface);
        assert_eq!(
            moved,
            vec![
                DamageBand {
                    y: 0,
                    height: raster.height
                },
                DamageBand {
                    y: 2 * raster.height,
                    height: raster.height
                },
            ]
        );

        // Hiding the cursor without any grid change still erases it.
        grid.cursor_visible = false;
        let hidden = raster.render_into(&grid, &[false; 3], &mut surface);
        assert_eq!(
            hidden,
            vec![DamageBand {
                y: 2 * raster.height,
                height: raster.height
            }]
        );
        // ...and once erased there is nothing left to erase.
        assert_eq!(raster.render_into(&grid, &[false; 3], &mut surface), vec![]);
    }

    #[test]
    fn invalidate_re_owes_the_surface_without_freeing_it() {
        let mut raster = raster();
        let mut surface = Surface::default();
        let grid = screen(4, 3, 'x');
        let _ = raster.render_into(&grid, &[], &mut surface);
        let capacity = surface.rgba.capacity();
        surface.invalidate();
        assert!(!surface.is_empty(), "invalidate keeps the allocation");
        let after = raster.render_into(&grid, &[false; 3], &mut surface);
        assert_eq!(
            after,
            vec![DamageBand {
                y: 0,
                height: 3 * raster.height
            }]
        );
        assert_eq!(surface.rgba.capacity(), capacity);
    }

    /// `render` is now `render_into` with a throwaway surface; a divergence
    /// between the two would silently change what bterm draws.
    #[test]
    fn render_matches_a_full_render_into() {
        let mut raster = raster();
        let mut grid = screen(6, 3, 'A');
        grid.cursor_visible = true;
        grid.cursor = (2, 1);
        let mut surface = Surface::default();
        let _ = raster.render_into(&grid, &[], &mut surface);
        assert_eq!(raster.render(&grid), surface.rgba());
    }

    /// Cold-review finding (2026-09-21): `Screen`'s fields are public, so a
    /// caller can hand over fewer cells than `cols * rows`. The loop then
    /// skipped the missing cells while `bands` still reported their row as
    /// repainted — a hidden cursor stayed on screen and no renderer could
    /// tell. The surface now shrinks to the rows that exist.
    #[test]
    fn a_short_cell_array_shrinks_the_surface_rather_than_lying_about_it() {
        let mut raster = raster();
        let mut surface = Surface::default();
        let mut grid = screen(3, 3, 'M');
        grid.cursor = (2, 2);
        grid.cursor_visible = true;
        let _ = raster.render_into(&grid, &[], &mut surface);
        assert_eq!(surface.grid(), (3, 3));

        // One cell short of the last row: that row cannot be painted, so it
        // must not be part of the surface or of the reported damage.
        grid.cells.truncate(8);
        grid.cursor_visible = false;
        let bands = raster.render_into(&grid, &[true; 3], &mut surface);
        assert_eq!(surface.grid(), (3, 2));
        assert_eq!(
            bands,
            vec![DamageBand {
                y: 0,
                height: 2 * raster.height
            }]
        );
        assert_eq!(
            surface.rgba().len(),
            surface.stride() * surface.height() as usize
        );

        // A malformed screen also forfeits the damage fast path: `dirty` is
        // indexed against the VT's row count, which no longer matches the
        // surface's, so every frame repaints whole rather than trusting a
        // slice that may be describing different rows.
        assert_eq!(
            raster.render_into(&grid, &[false; 3], &mut surface),
            vec![DamageBand {
                y: 0,
                height: 2 * raster.height
            }]
        );
        // Restore a well-formed screen and the fast path comes back.
        let grid = screen(3, 2, 'M');
        let _ = raster.render_into(&grid, &[], &mut surface);
        assert_eq!(raster.render_into(&grid, &[false; 2], &mut surface), vec![]);
    }

    #[test]
    fn an_empty_grid_produces_no_surface_and_no_damage() {
        let mut raster = raster();
        let mut surface = Surface::default();
        assert_eq!(raster.render_into(&screen(0, 0, ' '), &[], &mut surface), vec![]);
        assert!(surface.is_empty());
    }
}
