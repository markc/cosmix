use crate::terminal::Screen;
use std::{collections::HashMap, path::PathBuf};
use swash::{
    FontRef,
    scale::{Render, ScaleContext, Source, image::Image},
    zeno::Format,
};

/// A contiguous run of damaged device-pixel rows.
///
/// `y` and `height` are in the target buffer's own physical pixels, which is
/// what `wgpu::Queue::write_texture`, a `wl_shm` damage rectangle and a Bevy
/// `Image`'s byte range all want. Full-width by construction: the raster's
/// damage granularity is the cell ROW, so a consumer that could use a
/// narrower rectangle gains nothing from this type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DamageBand {
    pub y: u32,
    pub height: u32,
}

impl DamageBand {
    /// Byte range of this band in a buffer whose rows are `stride` bytes.
    pub fn byte_range(&self, stride: usize) -> std::ops::Range<usize> {
        self.y as usize * stride..(self.y as usize + self.height as usize) * stride
    }
}

/// What [`Raster::paint`] must remember between frames about **one** target
/// buffer, for a caller that owns its pixels itself.
///
/// [`Surface`] is this plus the buffer. A frontend whose pixels must live
/// somewhere else — a Bevy `Image`'s own `Vec<u8>`, a `wl_shm` pool mapping —
/// keeps one of these beside that buffer and calls `paint` directly, so
/// neither frontend copies a frame to reach its renderer.
///
/// The state is per-target, not per-[`Raster`]: two panes share one glyph
/// cache and must not share damage, geometry or cursor bookkeeping.
#[derive(Default)]
pub struct PaintState {
    cols: usize,
    rows: usize,
    cell: (u32, u32),
    cursor: Option<(usize, usize)>,
    /// Identity of the buffer last painted; see [`Raster::paint`]'s note on
    /// what it can and cannot detect.
    buffer: (usize, usize),
    /// Scratch, reused so a frame costs no allocation.
    bands: Vec<DamageBand>,
    rows_scratch: Vec<bool>,
}

impl PaintState {
    /// Cells, as of the last paint.
    pub fn grid(&self) -> (usize, usize) {
        (self.cols, self.rows)
    }
    /// Forget what was drawn, so the next paint repaints every row.
    ///
    /// **Required** when the caller changes which terminal it is painting, or
    /// loses the target's contents (a recreated GPU texture, a new shm pool).
    /// `dirty` describes the NEW terminal's damage, and a terminal that has
    /// been sitting still reports nothing dirty.
    pub fn invalidate(&mut self) {
        self.cols = 0;
        self.rows = 0;
        self.cell = (0, 0);
        self.cursor = None;
        self.buffer = (0, 0);
    }
}

/// A persistent RGBA8 surface that owns its buffer.
///
/// The point of this type is what it is *not*: a `Vec<u8>` returned by value
/// per frame. `Raster::render` allocates one whole grid image on every call
/// (~12 MB at 2.5x scale), which is what made the Bevy frontend build a brand
/// new `Image` per damaged frame. A frontend owns one of these per pane for
/// the pane's life and re-rasterises only the rows the grid reports dirty.
///
/// Owning the buffer is what makes that churn unrepresentable *for this
/// path*: a `Surface` user cannot hand in a fresh allocation per frame. A
/// caller whose pixels must live elsewhere uses [`Raster::paint`] with its own
/// [`PaintState`] instead — same painting code, different owner.
#[derive(Default)]
pub struct Surface {
    state: PaintState,
    rgba: Vec<u8>,
    width: u32,
    height: u32,
}

impl Surface {
    /// RGBA8, row-major, tightly packed at `width * 4` bytes per row.
    ///
    /// Straight alpha, not premultiplied — and moot either way, because the
    /// raster writes 255 into every alpha byte. Channel order is R, G, B, A
    /// in ascending address order, sRGB-**encoded** (the VT's palette is
    /// 8-bit sRGB, not linear light).
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
        self.state.grid()
    }
    pub fn is_empty(&self) -> bool {
        self.rgba.is_empty()
    }
    /// Forget the drawn content without freeing the allocation, so the next
    /// `render_into` repaints every row. See [`PaintState::invalidate`] for
    /// when a caller MUST do this.
    pub fn invalidate(&mut self) {
        self.state.invalidate();
    }
}

/// Whole cell rows the screen actually has cells for.
///
/// `Screen`'s fields are public, so a cell array shorter than `cols * rows`
/// is constructible even though `Terminal::capture` never produces one. The
/// painter paints only the rows that exist, so a returned band can never
/// claim a row the loop skipped.
fn paintable_rows(screen: &Screen) -> usize {
    screen.rows.min(screen.cells.len() / screen.cols.max(1))
}

/// Physical pixels a target must be able to hold, and the cell rows behind
/// them. The one place this arithmetic exists.
fn target(raster: &Raster, screen: &Screen) -> (usize, usize, usize) {
    let rows = paintable_rows(screen);
    (
        screen.cols * raster.width as usize,
        rows * raster.height as usize,
        rows,
    )
}

/// Runs of `true` in `rows`, in device-pixel coordinates, into `out`.
fn bands_into(rows: &[bool], cell_height: u32, out: &mut Vec<DamageBand>) {
    out.clear();
    let mut start: Option<usize> = None;
    for (index, dirty) in rows.iter().chain(std::iter::once(&false)).enumerate() {
        match (dirty, start) {
            (true, None) => start = Some(index),
            (false, Some(first)) => {
                out.push(DamageBand {
                    y: first as u32 * cell_height,
                    height: (index - first) as u32 * cell_height,
                });
                start = None;
            }
            _ => {}
        }
    }
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
        std::mem::take(&mut surface.rgba)
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
    /// Physical pixels a [`Raster::paint`] target must be able to hold for
    /// `screen`, at this raster's cell size.
    ///
    /// **A caller that owns its own buffer sizes it from here**, never from
    /// its own `cols * cell` arithmetic: the row count is clamped to the
    /// whole rows the screen actually has cells for, and a caller that
    /// reimplemented that clamp slightly differently would have its buffer
    /// refused — or, worse, would drift from the painter one edit later. The
    /// minimum stride is `width * 4`; the minimum length is
    /// `stride * height`.
    pub fn target_size(&self, screen: &Screen) -> (u32, u32) {
        let (width, height, _) = target(self, screen);
        (width as u32, height as u32)
    }

    pub fn render_into<'a>(
        &mut self,
        screen: &Screen,
        dirty: &[bool],
        surface: &'a mut Surface,
    ) -> &'a [DamageBand] {
        let (width, height, rows) = target(self, screen);
        if screen.cols == 0 || rows == 0 {
            surface.rgba.clear();
            surface.width = 0;
            surface.height = 0;
            surface.state.invalidate();
            surface.state.bands.clear();
            return &surface.state.bands;
        }
        let bytes = width * height * 4;
        // Resizing is this wrapper's whole job; `paint` never touches the
        // caller's allocation. A grown buffer also changes its identity, so
        // `paint` repaints it whole without being told.
        if surface.rgba.len() != bytes {
            surface.rgba.clear();
            surface.rgba.resize(bytes, 0);
        }
        surface.width = width as u32;
        surface.height = height as u32;
        let Surface { state, rgba, .. } = surface;
        self.paint(screen, rgba, width * 4, state, dirty)
    }

    /// The painting implementation, writing into a buffer the **caller**
    /// owns. [`Raster::render_into`] is this with the buffer owned for you;
    /// there is exactly one copy of the glyph loop and both frontends run it.
    ///
    /// `stride` is the target's row length in bytes, so a caller whose rows
    /// are padded (a `wl_shm` pool, a texture with an alignment requirement)
    /// paints straight into it. `dst` must be at least `stride * height`
    /// bytes and `stride` at least `width * 4`, where `width` and `height`
    /// come from the grid and the cell size; a buffer that is not is
    /// **refused** — no bands, nothing written, state invalidated — because
    /// nothing here can resize a buffer it does not own.
    ///
    /// Pixel format is [`Surface::rgba`]'s: straight RGBA8, sRGB-encoded,
    /// alpha always 255.
    ///
    /// ## What the caller must guarantee
    ///
    /// **The same buffer, frame after frame.** Damage-bounded painting means
    /// the rows this call skips keep whatever the target already held. The
    /// state records the buffer's address and length and forces a full
    /// repaint when either changes, which catches the ordinary cases — but an
    /// allocator may hand back the same address and length for a genuinely
    /// different buffer, so it is a mitigation and not a guarantee. On any
    /// doubt, and whenever the painted terminal changes, call
    /// [`PaintState::invalidate`].
    pub fn paint<'a>(
        &mut self,
        screen: &Screen,
        dst: &mut [u8],
        stride: usize,
        state: &'a mut PaintState,
        dirty: &[bool],
    ) -> &'a [DamageBand] {
        state.bands.clear();
        // `Screen`'s fields are public, so a cell array shorter than
        // `cols * rows` is constructible even though `Terminal::capture` never
        // produces one. Paint only the whole rows that actually exist: the
        // alternative is a returned band claiming a row was repainted while
        // the loop skipped it, which is a lie a renderer cannot detect and
        // which leaves the old pixels — including an old cursor — on screen.
        let cell = (self.width, self.height);
        let (width, height, rows) = target(self, screen);
        if screen.cols == 0
            || rows == 0
            || stride < width * 4
            || dst.len() < stride * height
        {
            state.invalidate();
            return &state.bands;
        }
        let buffer = (dst.as_ptr() as usize, dst.len());
        // The state's own record decides, never the caller's: a Raster
        // rebuilt at a new scale changes `cell` while cols/rows stay put, and
        // that must still force a full repaint.
        let full = state.cols != screen.cols
            || state.rows != rows
            || state.cell != cell
            || state.buffer != buffer;
        if full {
            state.cols = screen.cols;
            state.rows = rows;
            state.cell = cell;
            state.buffer = buffer;
            state.cursor = None;
        }
        let dirty_rows = &mut state.rows_scratch;
        dirty_rows.clear();
        dirty_rows.resize(rows, full);
        if !full {
            // `dirty` is indexed against the VT's row count; a short cell
            // array shrinks the painted area but not the snapshot, so the
            // slice is only trustworthy when both agree.
            if dirty.len() == screen.rows && screen.rows == rows {
                dirty_rows.copy_from_slice(dirty);
            } else {
                dirty_rows.fill(true);
            }
            if let Some((_, previous)) = state.cursor
                && previous < rows
            {
                dirty_rows[previous] = true;
            }
            if screen.cursor_visible && screen.cursor.1 < rows {
                dirty_rows[screen.cursor.1] = true;
            }
        }
        let rgba = dst;
        for (i, cell) in screen.cells.iter().take(screen.cols * rows).enumerate() {
            if !dirty_rows[i / screen.cols] {
                continue;
            }
            let x = (i % screen.cols) as i32 * self.width as i32;
            let y = (i / screen.cols) as i32 * self.height as i32;
            for cy in 0..self.height as usize {
                for cx in 0..self.width as usize {
                    let offset = (y as usize + cy) * stride + (x as usize + cx) * 4;
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
                        let offset = dy as usize * stride + dx as usize * 4;
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
                    let offset = y * stride + x * 4;
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
        state.cursor = drawn.then_some(screen.cursor);
        bands_into(dirty_rows, self.height, &mut state.bands);
        &state.bands
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

    /// `bands_into` through a Vec, for asserting on runs directly.
    fn runs(rows: &[bool], cell_height: u32) -> Vec<DamageBand> {
        let mut out = Vec::new();
        bands_into(rows, cell_height, &mut out);
        out
    }

    /// Bands copied out, so the surface is readable in the same assertion.
    fn into(raster: &mut Raster, screen: &Screen, dirty: &[bool], surface: &mut Surface) -> Vec<DamageBand> {
        raster.render_into(screen, dirty, surface).to_vec()
    }

    #[test]
    fn runs_of_dirty_rows_coalesce_into_bands() {
        assert_eq!(runs(&[false, false], 10), vec![]);
        assert_eq!(
            runs(&[true, true, false, true], 10),
            vec![
                DamageBand { y: 0, height: 20 },
                DamageBand { y: 30, height: 10 },
            ]
        );
        // A run that reaches the last row must still be closed.
        assert_eq!(
            runs(&[false, true, true], 4),
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
        let bands = raster.render_into(&grid, &[true; 3], &mut surface).to_vec();
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

    /// The property the whole damage model rests on: however a frame is
    /// reached — one full repaint, or a sequence of partial ones — the pixels
    /// are identical. Anything that breaks damage bookkeeping breaks this.
    #[test]
    fn a_sequence_of_partial_repaints_equals_one_full_repaint() {
        let mut incremental = raster();
        let mut surface = Surface::default();
        let mut grid = screen(10, 5, ' ');
        grid.cursor_visible = true;
        let _ = into(&mut incremental, &grid, &[], &mut surface);

        // Walk a cursor down the screen, changing one row's text each step,
        // telling the raster only about that row — exactly what the VT does.
        for row in 0..5 {
            for column in 0..10 {
                grid.cells[row * 10 + column].c =
                    char::from(b'a' + ((row * 10 + column) % 26) as u8);
            }
            grid.cursor = (row, row);
            let mut dirty = vec![false; 5];
            dirty[row] = true;
            let _ = into(&mut incremental, &grid, &dirty, &mut surface);
        }

        // The same final screen, painted once from nothing.
        let mut whole = raster();
        assert_eq!(
            surface.rgba(),
            whole.render(&grid),
            "an incremental sequence diverged from a full repaint"
        );
    }

    /// `paint` writes into a buffer it does not own, so — unlike `Surface` —
    /// a wrong-sized one is representable. It must refuse rather than write
    /// out of range or report bands it did not write.
    #[test]
    fn paint_refuses_a_buffer_it_cannot_fill() {
        let mut raster = raster();
        let mut state = PaintState::default();
        let grid = screen(4, 3, 'x');
        let width = 4 * raster.width as usize;
        let height = 3 * raster.height as usize;
        let stride = width * 4;

        for (label, len, stride) in [
            ("one byte short", stride * height - 1, stride),
            ("stride narrower than a row", stride * height, stride - 4),
            ("empty", 0, stride),
        ] {
            let mut dst = vec![0x5a_u8; len];
            let bands = raster.paint(&grid, &mut dst, stride, &mut state, &[]);
            assert!(bands.is_empty(), "{label}: reported damage it did not write");
            assert!(
                dst.iter().all(|byte| *byte == 0x5a),
                "{label}: wrote into a buffer it should have refused"
            );
        }

        // A correctly sized buffer is painted, and a padded stride is
        // honoured rather than assumed away: the pad bytes stay untouched.
        let pad = 16;
        let padded = stride + pad;
        let mut dst = vec![0x5a_u8; padded * height];
        let bands = raster.paint(&grid, &mut dst, padded, &mut state, &[]).to_vec();
        assert_eq!(
            bands,
            vec![DamageBand {
                y: 0,
                height: height as u32
            }]
        );
        for row in 0..height {
            let tail = row * padded + stride..row * padded + padded;
            assert!(
                dst[tail].iter().all(|byte| *byte == 0x5a),
                "row {row}: painted over the caller's row padding"
            );
        }
        assert!(dst[..stride].iter().any(|byte| *byte != 0x5a));
    }

    /// `paint` refuses a buffer it cannot fill, so a caller that owns its
    /// pixels needs the size rule exactly — including the clamp to whole
    /// rows. `target_size` IS that rule, and a buffer sized from it must
    /// never be refused. If these two ever diverge, a Bevy `Image` sized by
    /// hand starts getting silently blank frames.
    #[test]
    fn a_buffer_sized_from_target_size_is_never_refused() {
        let mut raster = raster();
        let mut state = PaintState::default();
        for (cols, rows, truncate) in [
            (1_usize, 1_usize, 0_usize),
            (4, 3, 0),
            (80, 24, 0),
            // A short cell array: the clamp is the part a caller would get
            // wrong, so it is the part worth pinning.
            (4, 3, 1),
            (10, 5, 12),
        ] {
            let mut grid = screen(cols, rows, 'x');
            grid.cells.truncate(grid.cells.len() - truncate);
            let (width, height) = raster.target_size(&grid);
            let stride = width as usize * 4;
            let mut dst = vec![0_u8; stride * height as usize];
            state.invalidate();
            let bands = raster
                .paint(&grid, &mut dst, stride, &mut state, &[])
                .to_vec();
            // Empty bands mean it refused the buffer; a different extent
            // means the two disagree about how big the frame is. Both are
            // the same bug and both fail here.
            assert_eq!(
                bands,
                vec![DamageBand { y: 0, height }],
                "{cols}x{rows} less {truncate} cells: target_size and paint disagree"
            );
            // And `Surface` agrees with the number it hands a slice caller.
            let mut surface = Surface::default();
            let _ = raster.render_into(&grid, &[], &mut surface);
            assert_eq!((surface.width(), surface.height()), (width, height));
            assert_eq!(surface.stride(), stride);
            assert_eq!(surface.rgba(), dst);
        }
    }

    /// Damage-bounded painting keeps whatever the target already held in the
    /// rows it skips, so a caller that swaps the buffer underneath must get
    /// everything back — not a half-drawn frame.
    #[test]
    fn paint_repaints_whole_when_the_caller_swaps_the_buffer() {
        let mut raster = raster();
        let mut state = PaintState::default();
        let grid = screen(4, 3, 'x');
        let stride = 4 * raster.width as usize * 4;
        let height = 3 * raster.height as usize;

        let mut first = vec![0_u8; stride * height];
        let _ = raster.paint(&grid, &mut first, stride, &mut state, &[]);

        // A different allocation, same geometry, nothing dirty.
        let mut second = vec![0x5a_u8; stride * height];
        let bands = raster
            .paint(&grid, &mut second, stride, &mut state, &[false; 3])
            .to_vec();
        assert_eq!(
            bands,
            vec![DamageBand {
                y: 0,
                height: height as u32
            }],
            "a new buffer must be owed the whole frame"
        );
        assert!(!second.contains(&0x5a));
        assert_eq!(first, second);
    }

    /// The cursor is drawn by INVERTING, so it must only ever land on a row
    /// this call repainted — otherwise the second of two calls inverts an
    /// already-inverted cell and the cursor vanishes. Painting is a full
    /// overwrite of the rows it touches, so repeating a paint is a no-op.
    #[test]
    fn a_repaint_is_idempotent_and_the_cursor_only_inverts_a_painted_row() {
        let mut raster = raster();
        let mut state = PaintState::default();
        let mut grid = screen(4, 3, 'M');
        grid.cursor_visible = true;
        grid.cursor = (1, 1);
        let stride = 4 * raster.width as usize * 4;
        let height = 3 * raster.height as usize;

        let mut once = vec![0_u8; stride * height];
        let _ = raster.paint(&grid, &mut once, stride, &mut state, &[]);
        let after_first = once.clone();

        // Same screen, every row dirty, same buffer: the cursor row is
        // repainted before it is inverted, so the result must not move.
        let _ = raster.paint(&grid, &mut once, stride, &mut state, &[true; 3]);
        assert_eq!(once, after_first, "repainting the same frame changed it");

        // The cursor's row must differ from its neighbours, or the inversion
        // is not happening at all and this test proves nothing.
        let band = raster.height as usize * stride;
        let row = move |buffer: &[u8], r: usize| buffer[r * band..(r + 1) * band].to_vec();
        assert_ne!(row(&once, 1), row(&once, 0));
        assert_eq!(row(&once, 0), row(&once, 2));

        // Move it with nothing else dirty: the row it left comes back to
        // exactly what an uninverted row looks like, and the new one inverts.
        grid.cursor = (1, 2);
        let _ = raster.paint(&grid, &mut once, stride, &mut state, &[false; 3]);
        assert_eq!(row(&once, 1), row(&once, 0), "the old cursor was not erased");
        assert_ne!(row(&once, 2), row(&once, 0));
    }

    #[test]
    fn an_empty_grid_produces_no_surface_and_no_damage() {
        let mut raster = raster();
        let mut surface = Surface::default();
        assert_eq!(raster.render_into(&screen(0, 0, ' '), &[], &mut surface), vec![]);
        assert!(surface.is_empty());
    }
}
