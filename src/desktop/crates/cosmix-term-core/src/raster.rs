use crate::terminal::{Cell, CellWidth, Screen};
#[path = "primary_font.rs"]
mod primary_font;
#[cfg(test)]
#[path = "primary_font_tests.rs"]
mod primary_font_tests;
#[path = "unicode_raster.rs"]
mod unicode;
use unicode::{Fonts, Pixels, UnicodeRaster};
#[cfg(test)]
#[path = "raster_unicode_tests.rs"]
mod unicode_tests;
use std::{collections::HashMap, path::Path, sync::Arc};
use swash::{
    scale::{Render, ScaleContext, Source, image::Image},
    zeno::Format,
};

/// A damaged rectangle in the target buffer's own physical pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DamageBand {
    pub x: u32,
    pub width: u32,
    pub y: u32,
    pub height: u32,
}

impl DamageBand {
    /// Conservative FULL-ROW byte range, including padding, for consumers
    /// that cannot upload horizontal ranges. This is not a packed rectangle.
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
    format: PixelFormat,
    cols: usize,
    rows: usize,
    cell: (u32, u32),
    cursor: Option<(usize, usize)>,
    cursor_span: usize,
    clusters: Option<Arc<()>>,
    display_offset: usize,
    stride: usize,
    scale: u32,
    raster: Option<Arc<()>>,
    cells: Vec<Cell>,
    cells_scratch: Vec<bool>,
    /// Identity of the buffer last painted; see [`Raster::paint`]'s note on
    /// what it can and cannot detect.
    buffer: (usize, usize),
    /// Scratch, reused so a frame costs no allocation.
    bands: Vec<DamageBand>,
    #[cfg(test)]
    rows_scratch: Vec<bool>,
}

/// Channel order at the destination boundary. Both paths paint opaque sRGB
/// pixels, so BGRA is also premultiplied BGRA without a conversion pass.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PixelFormat {
    #[default]
    Rgba,
    Bgra,
}

impl PixelFormat {
    fn colour(self, rgb: [u8; 3]) -> [u8; 3] {
        match self {
            Self::Rgba => rgb,
            Self::Bgra => [rgb[2], rgb[1], rgb[0]],
        }
    }
}

impl PaintState {
    /// Cells, as of the last paint.
    pub fn grid(&self) -> (usize, usize) {
        (self.cols, self.rows)
    }

    /// Retarget incremental painting to a byte-for-byte copy of the last
    /// painted buffer, preserving geometry, cursor and damage bookkeeping.
    ///
    /// The caller must copy the entire buffer, including any stride padding,
    /// without changing its layout or contents before calling this method.
    /// Byte equality cannot be checked here: the old allocation may be gone.
    /// Subsequent geometry changes are still detected by [`Raster::paint`].
    /// An invalidated or never-painted state remains invalidated.
    pub fn rebind(&mut self, dst: &[u8]) {
        if self.cols == 0 || self.rows == 0 {
            return;
        }
        debug_assert_eq!(dst.len(), self.buffer.1, "rebind requires a complete copy");
        debug_assert!(
            dst.len() >= self.cols * self.rows * self.cell.0 as usize * self.cell.1 as usize * 4,
            "rebind must preserve the painted geometry"
        );
        self.buffer = (dst.as_ptr() as usize, dst.len());
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
        self.raster = None;
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

fn visible_cursor(screen: &Screen, rows: usize) -> Option<(usize, usize)> {
    (screen.cursor_visible && screen.cursor.0 < screen.cols && screen.cursor.1 < rows).then(|| {
        let (col, row) = screen.cursor;
        let cells = &screen.cells[row * screen.cols..(row + 1) * screen.cols];
        let col = if col > 0
            && cells[col].width == CellWidth::Spacer
            && cells[col - 1].width == CellWidth::Wide
        {
            col - 1
        } else {
            col
        };
        (col, row)
    })
}

fn cell_span(cells: &[Cell], col: usize) -> usize {
    if cells[col].width == CellWidth::Wide
        && cells
            .get(col + 1)
            .is_some_and(|c| c.width == CellWidth::Spacer)
    {
        2
    } else {
        1
    }
}

fn cursor_span(screen: &Screen, cursor: Option<(usize, usize)>) -> usize {
    cursor.map_or(0, |(col, row)| {
        cell_span(
            &screen.cells[row * screen.cols..(row + 1) * screen.cols],
            col,
        )
    })
}

/// Scalar rows retain the single cheap compare-and-record pass. At the first
/// width marker, defer recording the suffix until old AND new pair edges have
/// expanded damage. Oppositely shifted pairs can form a whole connected run.
#[inline]
fn compare_row(now: &[Cell], old: &mut [Cell], changed: &mut [bool], dirty: bool) -> usize {
    let mut count = 0;
    let mut paired = None;
    for (i, ((changed, now), old)) in changed.iter_mut().zip(now).zip(old.iter_mut()).enumerate() {
        if now.width != CellWidth::Narrow || old.width != CellWidth::Narrow {
            paired = Some(i);
            break;
        }
        if dirty && now != old {
            *old = *now;
            *changed = true;
        } else if *changed {
            *old = *now;
        }
        count += usize::from(*changed);
    }
    let Some(i) = paired else {
        return count;
    };
    for j in i..now.len() {
        changed[j] |= dirty && now[j] != old[j];
    }
    let edge = |j| cell_span(now, j) == 2 || cell_span(old, j) == 2;
    for j in i..now.len().saturating_sub(1) {
        if edge(j) && changed[j] {
            changed[j + 1] = true;
        }
    }
    for j in (i..now.len().saturating_sub(1)).rev() {
        if edge(j) && changed[j + 1] {
            changed[j] = true;
        }
    }
    for j in i..now.len() {
        if changed[j] {
            old[j] = now[j];
            count += 1;
        }
    }
    count
}

/// Horizontal cell runs; merge vertically only when the x extents match.
fn cell_bands_into(changed: &[bool], cols: usize, cell: (u32, u32), out: &mut Vec<DamageBand>) {
    out.clear();
    let limit = 2 * (changed.len() / cols);
    // Indices, sorted by x, of runs touching the preceding row. A run can
    // start many rows earlier; searching all emitted rectangles is quadratic.
    let mut previous: Vec<usize> = Vec::new();
    let mut current = Vec::new();
    for (row, cells) in changed.chunks_exact(cols).enumerate() {
        current.clear();
        let mut candidate = 0;
        let mut col = 0;
        while col < cols {
            if !cells[col] {
                col += 1;
                continue;
            }
            let first = col;
            while col < cols && cells[col] {
                col += 1;
            }
            let x = first as u32 * cell.0;
            let width = (col - first) as u32 * cell.0;
            let y = row as u32 * cell.1;
            while candidate < previous.len() && out[previous[candidate]].x < x {
                candidate += 1;
            }
            if let Some(&index) = previous.get(candidate)
                && out[index].x == x
                && out[index].width == width
            {
                out[index].height += cell.1;
                current.push(index);
            } else {
                current.push(out.len());
                out.push(DamageBand {
                    x,
                    width,
                    y,
                    height: cell.1,
                });
                if out.len() > limit {
                    // Dithered damage is cheaper as conservative row bands.
                    out.clear();
                    for (row, cells) in changed.chunks_exact(cols).enumerate() {
                        if !cells.iter().any(|&changed| changed) {
                            continue;
                        }
                        let y = row as u32 * cell.1;
                        if let Some(last) = out.last_mut()
                            && last.y + last.height == y
                        {
                            last.height += cell.1;
                        } else {
                            out.push(DamageBand {
                                x: 0,
                                y,
                                width: cols as u32 * cell.0,
                                height: cell.1,
                            });
                        }
                    }
                    return;
                }
            }
        }
        std::mem::swap(&mut previous, &mut current);
    }
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
#[cfg(test)]
fn bands_into(rows: &[bool], width: u32, cell_height: u32, out: &mut Vec<DamageBand>) {
    out.clear();
    let mut start: Option<usize> = None;
    for (index, dirty) in rows.iter().chain(std::iter::once(&false)).enumerate() {
        match (dirty, start) {
            (true, None) => start = Some(index),
            (false, Some(first)) => {
                out.push(DamageBand {
                    x: 0,
                    width,
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
    identity: Arc<()>,
    cursor: crate::config::Cursor,
    /// The font file, shared with every raster [`Raster::resized`] from this
    /// one: a zoom step reuses the bytes instead of reading the file again.
    data: Arc<[u8]>,
    context: ScaleContext,
    cache: HashMap<(char, bool, [u8; 3]), Option<Image>>,
    unicode: UnicodeRaster,
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
        let override_path = std::env::var_os("TERM_SPIKE_FONT");
        let primary = primary_font::discover(override_path.as_deref().map(Path::new))?;
        let raster = Self::from_font(primary.data, primary.index, scale, logical_px, cursor)?;
        eprintln!(
            "DIAGNOSTIC font={} face={} scale={} cell={}x{} (physical px); bold=regular+brighter colour",
            primary.path.display(),
            primary.index,
            raster.scale,
            raster.width,
            raster.height
        );
        Ok(raster)
    }

    /// Fixed DejaVu face for pixel oracles and repeatable benchmarks. Ignores
    /// desktop discovery and process environment; requires the free fixture.
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test(
        scale: f32,
        logical_px: f32,
        cursor: crate::config::Cursor,
    ) -> Result<Self, String> {
        let primary = primary_font::fixture()?;
        Self::from_font(primary.data, primary.index, scale, logical_px, cursor)
    }

    /// The same font at another scale or size: no file read and no
    /// DIAGNOSTIC line. This is the runtime path — a held Ctrl+= steps the
    /// size at key-repeat rate. This preserves font identities and avoids
    /// override file reads and discovery diagnostics. The glyph cache starts empty,
    /// because every cached glyph was rasterised at the old size.
    pub fn resized(&self, scale: f32, logical_px: f32) -> Result<Self, String> {
        Self::from_fonts(
            self.data.clone(),
            self.unicode.fonts.clone(),
            scale,
            logical_px,
            self.cursor,
        )
    }

    fn from_font(
        data: Arc<[u8]>,
        index: u32,
        scale: f32,
        logical_px: f32,
        cursor: crate::config::Cursor,
    ) -> Result<Self, String> {
        let fonts = Fonts::discover(data.clone(), index).ok_or("Invalid primary font")?;
        Self::from_fonts(data, fonts, scale, logical_px, cursor)
    }

    fn from_fonts(
        data: Arc<[u8]>,
        fonts: Arc<Fonts>,
        scale: f32,
        logical_px: f32,
        cursor: crate::config::Cursor,
    ) -> Result<Self, String> {
        let scale = scale.clamp(0.5, 8.0);
        // Startup resolves logical size once; scale makes it physical for HiDPI.
        let px = logical_px * scale;
        let font = fonts.primary.font();
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
        Ok(Self {
            identity: Arc::new(()),
            cursor,
            data,
            context: ScaleContext::new(),
            cache: HashMap::new(),
            unicode: UnicodeRaster::new(fonts),
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
    /// **No frontend uses this.** Both paint in place — bterm through
    /// [`Raster::paint`] into its Bevy `Image`, the iced one through
    /// [`Raster::render_into`] and a [`Surface`] — because every call here
    /// allocates `cols * rows * cell * 4` bytes, ~12 MB at 2.5x scale, and a
    /// streaming pane damages at up to 60 fps. It survives as the one-shot
    /// form for tests and for a caller that genuinely wants a whole frame
    /// once; reach for it in a render loop and you have reintroduced the
    /// churn the damage model exists to remove.
    pub fn render(&mut self, screen: &Screen) -> Vec<u8> {
        let mut surface = Surface::default();
        let _ = self.render_into(screen, &[], &mut surface);
        std::mem::take(&mut surface.rgba)
    }

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

    /// Geometry/font/viewport invalidation, independent of storage identity.
    /// A copied buffer may retain state only through `PaintState::rebind`.
    pub fn requires_full_paint(
        &self,
        screen: &Screen,
        state: &PaintState,
        format: PixelFormat,
    ) -> bool {
        state.cols != screen.cols
            || state.rows != paintable_rows(screen)
            || state.cell != (self.width, self.height)
            || state.format != format
            || state.scale != self.scale.to_bits()
            || state.display_offset != screen.display_offset
            || !state
                .clusters
                .as_ref()
                .is_some_and(|id| Arc::ptr_eq(id, &screen.clusters.identity))
            || !state
                .raster
                .as_ref()
                .is_some_and(|id| Arc::ptr_eq(id, &self.identity))
    }

    /// Read-only preflight for copy-on-write frontends. The caller must still
    /// own the last-painted bytes; no pixel allocation is acquired for a no-op.
    pub fn is_current(
        &self,
        screen: &Screen,
        state: &PaintState,
        dirty: &[bool],
        format: PixelFormat,
    ) -> bool {
        if self.requires_full_paint(screen, state, format)
            || screen.cols == 0
            || dirty.len() != screen.rows
            || paintable_rows(screen) != screen.rows
            || state.cursor != visible_cursor(screen, state.rows)
            || state.cursor_span != cursor_span(screen, visible_cursor(screen, state.rows))
        {
            return false;
        }
        screen.cells[..screen.cols * state.rows]
            .chunks_exact(screen.cols)
            .zip(state.cells.chunks_exact(screen.cols))
            .zip(dirty)
            .all(|((now, old), dirty)| !*dirty || now == old)
    }

    /// Whether painting is guaranteed to overwrite every cell. A dirty-row
    /// hint alone is insufficient: equal cells now retain their old pixels.
    pub fn overwrites_all(
        &self,
        screen: &Screen,
        state: &PaintState,
        dirty: &[bool],
        format: PixelFormat,
    ) -> bool {
        self.requires_full_paint(screen, state, format)
            || dirty.len() != screen.rows
            || paintable_rows(screen) != screen.rows
            || (dirty.iter().all(|&row| row)
                && screen
                    .cells
                    .iter()
                    .zip(&state.cells)
                    .all(|(now, old)| now != old))
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
    /// The returned bands cover the regions of `surface.rgba()` this call
    /// wrote; fragmented damage falls back to full-width row bands. An empty
    /// result means the surface already holds the frame
    /// and the caller owes the GPU (or the compositor) nothing at all, which
    /// is the idle case a terminal spends almost all of its life in.
    ///
    /// Dirty rows are hints: only cells whose captured visual attributes
    /// differ are repainted. Cursor moves/hide/show additionally repaint the
    /// old/new cursor cells. A stationary cursor is composited only if its
    /// underlying cell was repainted. Viewport changes force full repaint.
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
    /// A byte-for-byte copy can instead use [`PaintState::rebind`] to retain
    /// incremental painting at its new address.
    pub fn paint<'a>(
        &mut self,
        screen: &Screen,
        dst: &mut [u8],
        stride: usize,
        state: &'a mut PaintState,
        dirty: &[bool],
    ) -> &'a [DamageBand] {
        self.paint_format(screen, dst, stride, state, dirty, PixelFormat::Rgba)
    }

    /// One non-ASCII / wide / extended-cluster cell, kept out of the ASCII
    /// glyph loop so that loop stays tight.
    #[cold]
    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    fn paint_unicode_cell(
        &mut self,
        screen: &Screen,
        cells: &[Cell],
        col: usize,
        rgba: &mut [u8],
        stride: usize,
        y: usize,
        format: PixelFormat,
    ) {
        let cell = cells[col];
        let mut scalar = [0; 4];
        let text = if cell.extra == 0 {
            cell.c.encode_utf8(&mut scalar)
        } else {
            screen.clusters.get(cell.extra).unwrap_or("")
        };
        let span = cell_span(cells, col);
        let image = match self.unicode.get(
            text,
            span,
            self.px,
            (self.width, self.height),
            self.baseline,
        ) {
            Some(image) => image,
            None => self.unicode.image(
                text,
                span,
                self.px,
                (self.width, self.height),
                self.baseline,
            ),
        };
        paint_cluster(
            image,
            rgba,
            stride,
            col * self.width as usize,
            y,
            self.width as usize * span,
            self.height as usize,
            cell.fg,
            format,
        );
    }

    /// Like [`Self::paint`], with an explicit destination channel order.
    /// Changing format invalidates all rows, even at the same buffer address.
    pub fn paint_format<'a>(
        &mut self,
        screen: &Screen,
        dst: &mut [u8],
        stride: usize,
        state: &'a mut PaintState,
        dirty: &[bool],
        format: PixelFormat,
    ) -> &'a [DamageBand] {
        if state.format != format {
            state.invalidate();
            state.format = format;
        }
        state.bands.clear();
        // `Screen`'s fields are public, so a cell array shorter than
        // `cols * rows` is constructible even though `Terminal::capture` never
        // produces one. Paint only the whole rows that actually exist: the
        // alternative is a returned band claiming a row was repainted while
        // the loop skipped it, which is a lie a renderer cannot detect and
        // which leaves the old pixels — including an old cursor — on screen.
        let cell = (self.width, self.height);
        let (width, height, rows) = target(self, screen);
        if screen.cols == 0 || rows == 0 || stride < width * 4 || dst.len() < stride * height {
            state.invalidate();
            return &state.bands;
        }
        let buffer = (dst.as_ptr() as usize, dst.len());
        // The state's own record decides, never the caller's: a Raster
        // rebuilt at a new scale changes `cell` while cols/rows stay put, and
        // that must still force a full repaint.
        let full = self.requires_full_paint(screen, state, format)
            || state.buffer != buffer
            || state.stride != stride
            || dirty.len() != screen.rows
            || screen.rows != rows;
        if full {
            state.cols = screen.cols;
            state.rows = rows;
            state.cell = cell;
            state.buffer = buffer;
            state.cursor = None;
            state.display_offset = screen.display_offset;
            state.stride = stride;
            state.scale = self.scale.to_bits();
            state.raster = Some(self.identity.clone());
            state.clusters = Some(screen.clusters.identity.clone());
            if state.cells.len() != screen.cols * rows {
                state.cells.clear();
                state.cells.reserve(screen.cols * rows);
            }
        }
        let cursor = visible_cursor(screen, rows);
        let span = cursor_span(screen, cursor);
        let cursor_changed = cursor != state.cursor || span != state.cursor_span;
        let changed = &mut state.cells_scratch;
        if !full {
            changed.clear();
            changed.resize(screen.cols * rows, false);
            if cursor_changed {
                for (position, span) in [(state.cursor, state.cursor_span), (cursor, span)] {
                    if let Some((col, row)) = position {
                        changed[row * screen.cols + col..row * screen.cols + col + span].fill(true);
                    }
                }
            }
        }
        if full {
            state.bands.push(DamageBand {
                x: 0,
                y: 0,
                width: width as u32,
                height: height as u32,
            });
        }
        let rgba = dst;
        for (row, cells) in screen.cells[..screen.cols * rows]
            .chunks_exact(screen.cols)
            .enumerate()
        {
            let start = row * screen.cols;
            // A full paint never allocates, clears or reads the change mask.
            // Record each row while visiting it for painting.
            let (row_changed, full_row) = if full {
                if start == state.cells.len() {
                    state.cells.extend_from_slice(cells);
                } else {
                    state.cells[start..start + screen.cols].copy_from_slice(cells);
                }
                (&[][..], true)
            } else {
                let cursor_row = cursor_changed
                    && state
                        .cursor
                        .into_iter()
                        .chain(cursor)
                        .any(|(_, y)| y == row);
                if !dirty[row] && !cursor_row {
                    continue;
                }
                let row_changed = &mut changed[start..start + screen.cols];
                let count = compare_row(
                    cells,
                    &mut state.cells[start..start + screen.cols],
                    row_changed,
                    dirty[row],
                );
                if count == 0 {
                    continue;
                }
                (&row_changed[..], count == screen.cols)
            };
            let y = row * self.height as usize;
            // Fill both halves before a wide lead paints across them. Scalar
            // glyphs keep the original cell-local mask loop below.
            let mut first = 0;
            while first < cells.len() {
                if !full_row && !row_changed[first] {
                    first += 1;
                    continue;
                }
                let bg = cells[first].bg;
                let mut end = first + 1;
                while end < cells.len() && (full_row || row_changed[end]) && cells[end].bg == bg {
                    end += 1;
                }
                let left = first * self.width as usize * 4;
                let right = end * self.width as usize * 4;
                let pixel = destination_pixel(format.colour(bg));
                for cy in y..y + self.height as usize {
                    // [u8; 4] has byte alignment: arbitrary slice origins and
                    // odd padded strides are valid, without unsafe casts.
                    let (pixels, tail) =
                        rgba[cy * stride + left..cy * stride + right].as_chunks_mut::<4>();
                    debug_assert!(tail.is_empty());
                    pixels.fill(pixel);
                }
                first = end;
            }
            for (col, cell) in cells.iter().enumerate() {
                if !full_row && !row_changed[col] {
                    continue;
                }
                if matches!(cell.width, CellWidth::Spacer | CellWidth::LeadingSpacer) {
                    continue;
                }
                if cell.extra != 0 || !cell.c.is_ascii() || cell.width == CellWidth::Wide {
                    // Out of line and cold: inlining this branch into the
                    // ASCII loop cost ~0.55 ms per warm full paint (cbc3).
                    self.paint_unicode_cell(screen, cells, col, rgba, stride, y, format);
                    continue;
                }
                if cell.c == ' ' || cell.c == '\0' {
                    continue;
                }
                let key = (cell.c, cell.bold, cell.fg);
                if !self.cache.contains_key(&key) {
                    if self.cache.len() >= 4096 {
                        self.cache.clear();
                    }
                    let font = self.unicode.fonts.primary.font();
                    let mut scaler = self.context.builder(font).size(self.px).hint(true).build();
                    let glyph = Render::new(&[Source::Outline])
                        .format(Format::Alpha)
                        .render(&mut scaler, font.charmap().map(cell.c));
                    self.cache.insert(key, glyph);
                }
                if let Some(glyph) = &self.cache[&key] {
                    let p = glyph.placement;
                    // Clip in cell-local coordinates once. i64 also avoids
                    // negation overflow for a negative placement bearing.
                    let left = i64::from(p.left);
                    let top = i64::from(self.baseline) - i64::from(p.top);
                    let gx0 = (-left).max(0);
                    let gy0 = (-top).max(0);
                    let gx1 = (i64::from(self.width) - left).min(i64::from(p.width));
                    let gy1 = (i64::from(self.height) - top).min(i64::from(p.height));
                    if gx0 >= gx1 || gy0 >= gy1 {
                        continue;
                    }
                    let count = (gx1 - gx0) as usize;
                    let x = col * self.width as usize + (left + gx0) as usize;
                    // Reorder colours once per glyph, never in the mask loop;
                    // the blend is per channel, so reordering first is exact.
                    // The cache key above keeps the original foreground.
                    let fg = format.colour(cell.fg).map(u32::from);
                    let bg = format.colour(cell.bg).map(u32::from);
                    let opaque = destination_pixel(format.colour(cell.fg));
                    for gy in gy0..gy1 {
                        let mask_start = gy as usize * p.width as usize + gx0 as usize;
                        let mask = &glyph.data[mask_start..mask_start + count];
                        let dy = y + (top + gy) as usize;
                        let offset = dy * stride + x * 4;
                        let (pixels, _) = rgba[offset..offset + count * 4].as_chunks_mut::<4>();
                        for (&alpha, pixel) in mask.iter().zip(pixels) {
                            match alpha {
                                0 => {}
                                255 => *pixel = opaque,
                                alpha => {
                                    let alpha = u32::from(alpha);
                                    let inverse = 255 - alpha;
                                    // Exactly the original integer expression.
                                    // This pixel still holds its cell's bg:
                                    // each mask sample visits it at most once.
                                    *pixel = destination_pixel(std::array::from_fn(|channel| {
                                        ((fg[channel] * alpha + bg[channel] * inverse) / 255) as u8
                                    }));
                                }
                            }
                        }
                    }
                }
            }
        }
        if !full {
            cell_bands_into(changed, screen.cols, cell, &mut state.bands);
        }
        // Steady cursor; invert a block so its glyph remains readable.
        if let Some((cx, cy)) = cursor
            && (full || changed[cy * screen.cols + cx])
        {
            let bottom = (cy + 1) * self.height as usize;
            let top = match self.cursor {
                crate::config::Cursor::Block => cy * self.height as usize,
                crate::config::Cursor::Underline => bottom - 1,
            };
            for y in top..bottom {
                for x in cx * self.width as usize..(cx + span) * self.width as usize {
                    let offset = y * stride + x * 4;
                    match self.cursor {
                        crate::config::Cursor::Block => {
                            for channel in &mut rgba[offset..offset + 3] {
                                *channel = 255 - *channel;
                            }
                        }
                        crate::config::Cursor::Underline => {
                            rgba[offset..offset + 4]
                                .copy_from_slice(&destination_pixel([220, 220, 220]));
                        }
                    }
                }
            }
        }
        state.cursor = cursor;
        state.cursor_span = span;
        &state.bands
    }
    // Frozen pre-rank-6 painter (with rank 3's per-cell colour reorder):
    // deliberately retains independent per-pixel writes and clipping as the
    // byte-equality oracle.
    #[cfg(test)]
    fn paint_reference<'a>(
        &mut self,
        screen: &Screen,
        dst: &mut [u8],
        stride: usize,
        state: &'a mut PaintState,
        dirty: &[bool],
        format: PixelFormat,
    ) -> &'a [DamageBand] {
        if state.format != format {
            state.invalidate();
            state.format = format;
        }
        state.bands.clear();
        // `Screen`'s fields are public, so a cell array shorter than
        // `cols * rows` is constructible even though `Terminal::capture` never
        // produces one. Paint only the whole rows that actually exist: the
        // alternative is a returned band claiming a row was repainted while
        // the loop skipped it, which is a lie a renderer cannot detect and
        // which leaves the old pixels — including an old cursor — on screen.
        let cell = (self.width, self.height);
        let (width, height, rows) = target(self, screen);
        if screen.cols == 0 || rows == 0 || stride < width * 4 || dst.len() < stride * height {
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
            // Reorder colours once per cell, not the fill/blend inner loops.
            // Keep the original foreground in the glyph cache key.
            let bg = format.colour(cell.bg);
            let fg = format.colour(cell.fg);
            for cy in 0..self.height as usize {
                for cx in 0..self.width as usize {
                    let offset = (y as usize + cy) * stride + (x as usize + cx) * 4;
                    rgba[offset..offset + 4].copy_from_slice(&[bg[0], bg[1], bg[2], 255]);
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
                let font = self.unicode.fonts.primary.font();
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
                            rgba[offset + channel] = ((fg[channel] as u32 * alpha
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
        bands_into(dirty_rows, width as u32, self.height, &mut state.bands);
        &state.bands
    }
}

/// Unicode layers blend over the destination, including both backgrounds.
#[allow(clippy::too_many_arguments)]
fn paint_cluster(
    image: &unicode::ClusterImage,
    dst: &mut [u8],
    stride: usize,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    fg: [u8; 3],
    format: PixelFormat,
) {
    let fg = format.colour(fg).map(u32::from);
    for layer in &image.layers {
        let left = i64::from(layer.x);
        let top = i64::from(layer.y);
        let x0 = (-left).max(0);
        let y0 = (-top).max(0);
        let x1 = (width as i64 - left).min(i64::from(layer.width));
        let y1 = (height as i64 - top).min(i64::from(layer.height));
        if x0 >= x1 || y0 >= y1 {
            continue;
        }
        for sy in y0..y1 {
            let start = (y + (top + sy) as usize) * stride + (x + (left + x0) as usize) * 4;
            let count = (x1 - x0) as usize;
            let (pixels, _) = dst[start..start + count * 4].as_chunks_mut::<4>();
            let source = sy as usize * layer.width as usize + x0 as usize;
            match &layer.pixels {
                Pixels::Mask(mask) => {
                    for (&a, pixel) in mask[source..source + count].iter().zip(pixels) {
                        if a == 0 {
                            continue;
                        }
                        if a == 255 {
                            *pixel = [fg[0] as u8, fg[1] as u8, fg[2] as u8, 255];
                            continue;
                        }
                        let a = u32::from(a);
                        for c in 0..3 {
                            pixel[c] = ((fg[c] * a + u32::from(pixel[c]) * (255 - a)) / 255) as u8;
                        }
                        pixel[3] = 255;
                    }
                }
                Pixels::Color(data) => {
                    for (src, pixel) in data[source * 4..(source + count) * 4]
                        .chunks_exact(4)
                        .zip(pixels)
                    {
                        if src[3] == 0 {
                            continue;
                        }
                        let rgb = format.colour([src[0], src[1], src[2]]);
                        let inverse = 255 - u32::from(src[3]);
                        for c in 0..3 {
                            pixel[c] = (u32::from(rgb[c]) + u32::from(pixel[c]) * inverse / 255)
                                .min(255) as u8;
                        }
                        pixel[3] = 255;
                    }
                }
            }
        }
    }
}

#[inline]
/// The RGB-to-destination pixel boundary for the optimised loops.
fn destination_pixel(rgb: [u8; 3]) -> [u8; 4] {
    [rgb[0], rgb[1], rgb[2], 255]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Cursor;
    use crate::terminal::Cell;
    use std::time::Instant;

    #[test]
    fn fully_changed_rows_record_cells_for_the_next_partial_paint() {
        for format in [PixelFormat::Rgba, PixelFormat::Bgra] {
            let mut raster = raster_with(Cursor::Block);
            let mut grid = screen(8, 3, 'M');
            grid.cursor_visible = true;
            grid.cursor = (3, 1);
            let (width, height) = raster.target_size(&grid);
            let stride = width as usize * 4 + 3;
            let mut pixels = vec![0x5a; stride * height as usize];
            let mut state = PaintState::default();
            raster.paint_format(&grid, &mut pixels, stride, &mut state, &[], format);
            for row in [None, Some(1)] {
                for (index, cell) in grid.cells.iter_mut().enumerate() {
                    if row.is_none_or(|row| index / grid.cols == row) {
                        cell.bg[0] = cell.bg[0].wrapping_add(1);
                    }
                }
                let dirty = [row.is_none(), true, row.is_none()];
                assert_eq!(
                    raster.paint_format(&grid, &mut pixels, stride, &mut state, &dirty, format),
                    &[DamageBand {
                        x: 0,
                        y: row.unwrap_or(0) as u32 * raster.height,
                        width,
                        height: if row.is_some() { raster.height } else { height },
                    }]
                );
                let mut expected = vec![0x5a; pixels.len()];
                raster.paint_reference(
                    &grid,
                    &mut expected,
                    stride,
                    &mut PaintState::default(),
                    &[],
                    format,
                );
                assert_eq!(pixels, expected);
                assert!(
                    raster
                        .paint_format(&grid, &mut pixels, stride, &mut state, &dirty, format)
                        .is_empty()
                );
            }
            grid.cells[11].c = 'g';
            assert_eq!(
                raster.paint_format(
                    &grid,
                    &mut pixels,
                    stride,
                    &mut state,
                    &[false, true, false],
                    format,
                ),
                &[DamageBand {
                    x: 3 * raster.width,
                    y: raster.height,
                    width: raster.width,
                    height: raster.height,
                }]
            );
            let mut expected = vec![0x5a; pixels.len()];
            raster.paint_reference(
                &grid,
                &mut expected,
                stride,
                &mut PaintState::default(),
                &[],
                format,
            );
            assert_eq!(pixels, expected);
        }
    }

    #[test]
    fn cell_ranges_skip_identical_visuals_and_include_selection_colours() {
        let mut raster = raster_with(Cursor::Block);
        let mut surface = Surface::default();
        let mut grid = screen(90, 4, 'M');
        grid.cursor_visible = true;
        grid.cursor = (7, 1);
        raster.render_into(&grid, &[], &mut surface);
        assert!(
            raster
                .render_into(&grid, &[true; 4], &mut surface)
                .is_empty()
        );
        let cell = &mut grid.cells[90 + 7];
        std::mem::swap(&mut cell.fg, &mut cell.bg);
        assert_eq!(
            raster.render_into(&grid, &[false, true, false, false], &mut surface),
            &[DamageBand {
                x: 7 * raster.width,
                y: raster.height,
                width: raster.width,
                height: raster.height
            }]
        );
        assert_eq!(surface.rgba(), raster.render(&grid));
        // Two disjoint runs on one row must not become a whole-row rectangle.
        grid.cells[91].c = 'g';
        grid.cells[170].bg = [201, 8, 31];
        assert_eq!(
            raster
                .render_into(&grid, &[false, true, false, false], &mut surface)
                .len(),
            2
        );
        assert_eq!(surface.rgba(), raster.render(&grid));
        let unchanged = surface.rgba().to_vec();
        assert!(
            raster
                .render_into(&grid, &[false; 4], &mut surface)
                .is_empty()
        );
        assert_eq!(surface.rgba(), unchanged);
    }

    #[test]
    fn randomized_incremental_frames_match_fresh_original_painter() {
        let mut seed = 0x5eed_1695_a37b_c201_u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for style in [Cursor::Block, Cursor::Underline] {
            let mut raster = raster_with(style);
            let mut grid = screen(13, 7, 'M');
            let mut state = PaintState::default();
            let mut pixels = Vec::new();
            let mut format = PixelFormat::Rgba;
            // First half retains the frozen ASCII oracle; second half runs
            // emoji/extras/pair transitions against the independent full painter.
            for frame in 0..4096 {
                let unicode_frames = frame >= 2048;
                let value = next();
                let mut dirty = vec![false; grid.rows];
                match frame % 16 {
                    0..=5 => {
                        let i = value as usize % grid.cells.len();
                        dirty[i / grid.cols] = true;
                        let cell = &mut grid.cells[i];
                        match frame % 16 {
                            0 => {
                                cell.c = [' ', '\0', 'g', '@', '~', '#'][(value >> 16) as usize % 6]
                            }
                            1 => cell.fg = [value as u8, (value >> 8) as u8, (value >> 24) as u8],
                            2 => cell.bg = [value as u8, (value >> 8) as u8, (value >> 32) as u8],
                            3 => cell.bold = !cell.bold,
                            4 => std::mem::swap(&mut cell.fg, &mut cell.bg),
                            _ => { /* redundant dirty row */ }
                        }
                    }
                    6 => {
                        grid.cursor = (
                            value as usize % (grid.cols + 1),
                            (value >> 16) as usize % (grid.rows + 1),
                        )
                    }
                    7 => grid.cursor_visible = !grid.cursor_visible,
                    8 => {
                        grid = screen(1 + value as usize % 19, 2 + (value >> 16) as usize % 7, 'M');
                        dirty = vec![false; grid.rows];
                    }
                    9 => {
                        let scale = [1.0, 1.25, 1.5, 2.5][value as usize % 4];
                        raster = raster.resized(scale, 13.0).unwrap();
                    }
                    10 => {
                        format = if format == PixelFormat::Rgba {
                            PixelFormat::Bgra
                        } else {
                            PixelFormat::Rgba
                        }
                    }
                    11 => grid.display_offset = if grid.display_offset == 0 { 3 } else { 0 },
                    12 => {
                        // An incomplete last row is ignored, but forces full
                        // repaint of the remaining whole rows on every call.
                        if grid.cells.len() == grid.cols * grid.rows {
                            grid.cells.pop();
                        }
                    }
                    13 => state.invalidate(),
                    14 => {
                        raster = raster
                            .resized(raster.scale, if value & 1 == 0 { 12.9 } else { 13.0 })
                            .unwrap()
                    }
                    _ => dirty.fill(true),
                }
                if unicode_frames && frame % 16 <= 5 {
                    unicode_tests::transition(&mut grid, &mut dirty, value, frame);
                }
                let (width, height) = raster.target_size(&grid);
                let stride = width as usize * 4 + 1 + (frame / 113) % 3;
                let origin = 7;
                let len = stride * height as usize;
                pixels.resize(origin + len + 11, 0x5a);
                if frame % 29 == 0 && state.buffer.1 == len && state.stride == stride {
                    // Explicit complete-copy rebind, including odd padding.
                    let copied = pixels.clone();
                    state.rebind(&copied[origin..origin + len]);
                    pixels = copied;
                }
                if frame % 37 == 0 {
                    let replacement = vec![0x5a; pixels.len()];
                    pixels = replacement; // changed allocation must invalidate
                }
                let before = pixels.clone();
                let mut expected = before.clone();
                let bands = raster
                    .paint_format(
                        &grid,
                        &mut pixels[origin..origin + len],
                        stride,
                        &mut state,
                        &dirty,
                        format,
                    )
                    .to_vec();
                if unicode_frames {
                    unicode_tests::paint_emoji_reference(
                        &raster,
                        &grid,
                        &mut expected[origin..origin + len],
                        stride,
                        format,
                    );
                } else {
                    raster.paint_reference(
                        &grid,
                        &mut expected[origin..origin + len],
                        stride,
                        &mut PaintState::default(),
                        &[],
                        format,
                    );
                }
                assert_eq!(
                    pixels, expected,
                    "frame={frame} style={style:?} format={format:?}"
                );
                for (index, (&old, &new)) in before.iter().zip(&pixels).enumerate() {
                    if old == new {
                        continue;
                    }
                    assert!(index >= origin && index < origin + len);
                    let offset = index - origin;
                    let (x, y) = ((offset % stride / 4) as u32, (offset / stride) as u32);
                    assert!(
                        bands.iter().any(|b| x >= b.x
                            && x < b.x + b.width
                            && y >= b.y
                            && y < b.y + b.height),
                        "unreported write frame={frame}"
                    );
                }
            }
        }
    }

    #[test]
    fn scrolled_visible_cursor_paints_pixels_in_both_formats() {
        for style in [Cursor::Block, Cursor::Underline] {
            for format in [PixelFormat::Rgba, PixelFormat::Bgra] {
                let mut raster = raster_with(style);
                let mut grid = screen(3, 3, ' ');
                grid.cursor = (1, 2);
                grid.cursor_visible = true;
                for cell in &mut grid.cells {
                    cell.bg = [10, 30, 70];
                }
                let (width, height) = raster.target_size(&grid);
                let stride = width as usize * 4;
                let mut pixels = vec![0; stride * height as usize];
                let mut state = PaintState::default();
                for offset in [0, 1, 2] {
                    grid.display_offset = offset;
                    raster.paint_format(
                        &grid,
                        &mut pixels,
                        stride,
                        &mut state,
                        &[false; 3],
                        format,
                    );
                    for y in 0..height {
                        for x in 0..width {
                            let in_cursor = x / raster.width == 1 && y / raster.height == 2;
                            let rgb = match style {
                                Cursor::Block if in_cursor => [245, 225, 185],
                                Cursor::Underline if in_cursor && y + 1 == height => [220; 3],
                                _ => [10, 30, 70],
                            };
                            let [r, g, b] = format.colour(rgb);
                            let start = y as usize * stride + x as usize * 4;
                            assert_eq!(
                                &pixels[start..start + 4],
                                &[r, g, b, 255],
                                "offset={offset} format={format:?} pixel=({x},{y})"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn full_paints_leave_change_scratch_untouched_and_record_cells() {
        let mut raster = raster_with(Cursor::Block);
        let mut surface = Surface::default();
        let mut grid = screen(7, 5, ' ');
        raster.render_into(&grid, &[], &mut surface);
        assert!(surface.state.cells_scratch.is_empty());
        for cell in &mut grid.cells {
            cell.bg = [11, 22, 33];
        }
        raster.render_into(&grid, &[true; 5], &mut surface);
        assert_eq!(surface.state.cells, grid.cells);
        surface.state.cells_scratch = vec![true, false, true];
        let scratch = surface.state.cells_scratch.clone();
        grid.cells[0].bg = [44, 55, 66];
        raster.render_into(&grid, &[], &mut surface);
        assert_eq!(surface.state.cells_scratch, scratch);
        assert_eq!(surface.state.cells, grid.cells);
        assert!(
            raster
                .render_into(&grid, &[true; 5], &mut surface)
                .is_empty()
        );
    }

    #[test]
    fn cell_band_merging_tracks_only_open_runs_and_bounds_dither() {
        let mut bands = Vec::new();
        // The left run starts before the right one and remains open for three
        // rows. Searching only the last appended entries would miss it.
        cell_bands_into(
            &[true, false, false, true, false, true, true, false, true],
            3,
            (2, 3),
            &mut bands,
        );
        assert_eq!(
            bands,
            vec![
                DamageBand {
                    x: 0,
                    y: 0,
                    width: 2,
                    height: 9
                },
                DamageBand {
                    x: 4,
                    y: 3,
                    width: 2,
                    height: 6
                },
            ]
        );
        let (cols, rows) = (96, 32);
        let changed: Vec<_> = (0..cols * rows)
            .map(|i| i / cols != 15 && (i / cols + i % cols) % 2 == 0)
            .collect();
        cell_bands_into(&changed, cols, (2, 3), &mut bands);
        assert!(bands.len() <= 2 * rows);
        assert_eq!(
            bands,
            vec![
                DamageBand {
                    x: 0,
                    y: 0,
                    width: 192,
                    height: 45
                },
                DamageBand {
                    x: 0,
                    y: 48,
                    width: 192,
                    height: 48
                },
            ]
        );
    }

    #[test]
    fn viewport_offset_forces_full_rows_even_when_cells_are_identical() {
        let mut raster = raster_with(Cursor::Block);
        let mut surface = Surface::default();
        let mut grid = screen(7, 5, 'M');
        grid.cursor_visible = true;
        grid.cursor = (1, 0);
        raster.render_into(&grid, &[], &mut surface);
        for offset in [1, 2, 0] {
            grid.display_offset = offset;
            let (width, height) = raster.target_size(&grid);
            assert_eq!(
                raster.render_into(&grid, &[false; 5], &mut surface),
                &[DamageBand {
                    x: 0,
                    width,
                    y: 0,
                    height
                }]
            );
            let mut expected = vec![0; surface.rgba().len()];
            raster.paint_reference(
                &grid,
                &mut expected,
                surface.stride(),
                &mut PaintState::default(),
                &[],
                PixelFormat::Rgba,
            );
            assert_eq!(surface.rgba(), expected);
            assert!(
                raster
                    .render_into(&grid, &[true; 5], &mut surface)
                    .is_empty()
            );
        }
    }

    #[test]
    fn destination_format_switch_repaints_clean_rows_and_preserves_padding() {
        for scale in [1.0, 1.25, 2.5] {
            for cursor in [Cursor::Block, Cursor::Underline] {
                let mut raster = Raster::for_test(scale, 13.0, cursor).unwrap();
                let mut grid = screen(3, 5, 'M');
                grid.cursor_visible = true;
                grid.cursor = (1, 4);
                for (i, cell) in grid.cells.iter_mut().enumerate() {
                    cell.fg = [255, i as u8 * 11, 0];
                    cell.bg = [3, 127, 253];
                }
                let (width, height) = raster.target_size(&grid);
                let stride = width as usize * 4 + 12;
                let mut bytes = vec![0x5a; stride * height as usize];
                let mut state = PaintState::default();
                raster.paint(&grid, &mut bytes, stride, &mut state, &[]);
                let rgba = bytes.clone();
                let damage = raster.paint_format(
                    &grid,
                    &mut bytes,
                    stride,
                    &mut state,
                    &[false; 5],
                    PixelFormat::Bgra,
                );
                assert_eq!(
                    damage,
                    &[DamageBand {
                        x: 0,
                        width,
                        y: 0,
                        height
                    }]
                );
                for (native, original) in bytes.chunks_exact(stride).zip(rgba.chunks_exact(stride))
                {
                    for (p, q) in native[..width as usize * 4]
                        .chunks_exact(4)
                        .zip(original[..width as usize * 4].chunks_exact(4))
                    {
                        assert_eq!(p, &[q[2], q[1], q[0], 255]);
                    }
                    assert_eq!(&native[width as usize * 4..], &[0x5a; 12]);
                }
                raster.paint(&grid, &mut bytes, stride, &mut state, &[false; 5]);
                assert_eq!(bytes, rgba, "default/wgpu destination must stay RGBA");
            }
        }
    }

    /// A zoom step must be the same raster `new` would build, without
    /// reading the font again: the bytes are shared, not re-read or copied.
    #[test]
    fn resized_matches_new_and_shares_the_font_bytes() {
        let base = Raster::for_test(1.0, 13.0, Cursor::Block).expect("DejaVu Sans Mono");
        for (scale, px) in [(1.0, 17.0), (2.5, 13.0), (1.25, 9.5)] {
            let resized = base.resized(scale, px).unwrap();
            let fresh = Raster::for_test(scale, px, Cursor::Block).unwrap();
            assert_eq!(
                (
                    resized.width,
                    resized.height,
                    resized.baseline,
                    resized.scale
                ),
                (fresh.width, fresh.height, fresh.baseline, fresh.scale),
                "{px} px @ {scale}"
            );
            assert_eq!(
                resized.cursor,
                Cursor::Block,
                "the cursor style carries over"
            );
            assert!(
                Arc::ptr_eq(&resized.data, &base.data),
                "the font file was read again"
            );
        }
    }

    pub(super) fn screen(cols: usize, rows: usize, fill: char) -> Screen {
        Screen {
            clusters: Default::default(),
            cols,
            rows,
            cursor: (0, 0),
            cursor_visible: false,
            display_offset: 0,
            cells: (0..cols * rows)
                .map(|_| Cell {
                    extra: 0,
                    width: Default::default(),
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
        raster_with(Cursor::Underline)
    }

    /// The cursor shape is not decoration in these tests: `Block` inverts the
    /// whole cell and `Underline` only its last row, so a test about
    /// inversion landing on the right row must say which one it means.
    pub(super) fn raster_with(cursor: Cursor) -> Raster {
        Raster::for_test(1.0, 13.0, cursor).expect("DejaVu Sans Mono")
    }

    /// Compare entire guarded allocations, including padding and untouched
    /// rows, as well as damage. The slice begins at a nonzero physical origin
    /// in a larger canvas; Raster itself has no origin argument.
    fn compare_painters(raster: &mut Raster, grid: &mut Screen, padding: usize) {
        // Both destination orders: the fast painter must honour the format
        // exactly as the reference does (a merge once dropped it silently).
        // The frame sequence mutates the grid, so restore it between passes.
        let cursor = (grid.cursor, grid.cursor_visible);
        let colours: Vec<_> = grid.cells.iter().map(|cell| (cell.fg, cell.bg)).collect();
        for format in [PixelFormat::Rgba, PixelFormat::Bgra] {
            (grid.cursor, grid.cursor_visible) = cursor;
            for (cell, &(fg, bg)) in grid.cells.iter_mut().zip(&colours) {
                cell.fg = fg;
                cell.bg = bg;
            }
            compare_painters_in(raster, grid, padding, format);
        }
    }

    fn compare_painters_in(
        raster: &mut Raster,
        grid: &mut Screen,
        padding: usize,
        format: PixelFormat,
    ) {
        let (width, height) = raster.target_size(grid);
        let stride = width as usize * 4 + padding + 28;
        let origin = 2 * stride + 12 + 1; // also deliberately byte-unaligned
        let len = stride * height as usize;
        let mut fast = vec![0x5a; origin + len + stride];
        let mut slow = fast.clone();
        let mut fast_state = PaintState::default();
        let mut slow_state = PaintState::default();
        for frame in 0..5 {
            if frame == 2 {
                grid.cursor = (grid.cols.saturating_sub(1), grid.rows.saturating_sub(1));
            }
            if frame == 3 {
                grid.cursor_visible = false;
            }
            let dirty: Vec<_> = (0..grid.rows)
                .map(|row| frame == 1 || (frame == 4 && row % 2 == 1))
                .collect();
            if frame == 4 {
                for (i, cell) in grid.cells.iter_mut().enumerate() {
                    if dirty[i / grid.cols] {
                        cell.bg = [17, 231, i as u8];
                        cell.fg = [255, 0, 128];
                    }
                }
            }
            let _bands = raster
                .paint_format(
                    grid,
                    &mut fast[origin..origin + len],
                    stride,
                    &mut fast_state,
                    &dirty,
                    format,
                )
                .to_vec();
            let _reference = raster.paint_reference(
                grid,
                &mut slow[origin..origin + len],
                stride,
                &mut slow_state,
                &dirty,
                format,
            );
            assert_eq!(
                fast, slow,
                "pixels frame={frame} scale={} {format:?}",
                raster.scale
            );
            assert!(fast[..origin].iter().all(|&b| b == 0x5a));
            assert!(fast[origin + len..].iter().all(|&b| b == 0x5a));
            for row in 0..height as usize {
                let pad = origin + row * stride + width as usize * 4;
                assert!(
                    fast[pad..origin + (row + 1) * stride]
                        .iter()
                        .all(|&b| b == 0x5a)
                );
            }
        }
    }

    #[test]
    fn span_and_mask_paint_matches_reference_on_varied_grids() {
        let base = raster();
        let mut seed = 0x6a09_e667_f3bc_c909_u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        // Frozen ASCII oracle. Unicode frames have a separate full painter.
        let chars = [' ', '\0', 'M', 'j', 'g', 'W', '@', '.', '~', '#'];
        for scale in [1.0, 1.25, 1.5, 2.5] {
            for cursor in [Cursor::Block, Cursor::Underline] {
                let mut raster = base.resized(scale, 13.0).unwrap();
                raster.cursor = cursor;
                for case in 0..16 {
                    let cols = 1 + (next() % 23) as usize;
                    let rows = 2 + (next() % 6) as usize;
                    let mut grid = screen(cols, rows, ' ');
                    let mut bg = [0, 0, 0];
                    for (i, cell) in grid.cells.iter_mut().enumerate() {
                        let value = next();
                        // Uniform rows, alternating backgrounds, and mixed runs.
                        if i % cols == 0 || case % 3 == 1 || (case % 3 == 2 && value % 5 == 0) {
                            bg = [value as u8, (value >> 8) as u8, (value >> 16) as u8];
                        }
                        cell.bg = bg;
                        cell.fg = [
                            (value >> 24) as u8,
                            (value >> 32) as u8,
                            (value >> 40) as u8,
                        ];
                        cell.bold = value & 1 != 0;
                        cell.c = chars[(value >> 48) as usize % chars.len()];
                    }
                    grid.cursor_visible = true;
                    grid.cursor = (cols / 2, rows / 2);
                    if case % 4 == 0 {
                        grid.cells.pop(); // partial final rows must still be ignored
                    }
                    compare_painters(&mut raster, &mut grid, case % 4);
                }
            }
        }
    }

    #[test]
    fn mask_endpoints_rounding_and_clipping_match_reference() {
        let mut raster = raster();
        raster.baseline = 0;
        // Synthetic masks guarantee coverage of all alpha values and all
        // clipping directions independently of the installed font's outlines.
        for (width, height) in [(256, 1), (7, 5)] {
            raster.width = width;
            raster.height = height;
            for (left, top) in [
                (0, 0),
                (-3, 2),
                (3, -2),
                (-300, 0),
                (300, 0),
                (0, 300),
                (0, -300),
            ] {
                for (fg, bg) in [
                    ([0, 255, 127], [255, 0, 128]),
                    ([1, 17, 254], [254, 239, 1]),
                    ([37, 37, 37], [37, 37, 37]),
                ] {
                    let mut glyph = Image::default();
                    glyph.placement.left = left;
                    glyph.placement.top = top;
                    glyph.placement.width = 256;
                    glyph.placement.height = 9;
                    // Each row is a different rotation of 0..=255, so every row
                    // still holds all alpha values but sampling the wrong source
                    // row under vertical clipping changes the output.
                    glyph.data = (0..9usize)
                        .flat_map(|row| (0..256usize).map(move |col| (col + row * 37) as u8))
                        .collect();
                    raster.cache.insert(('M', true, fg), Some(glyph));
                    let mut grid = screen(3, 3, 'M');
                    for cell in &mut grid.cells {
                        cell.fg = fg;
                        cell.bg = bg;
                        cell.bold = true;
                    }
                    compare_painters(&mut raster, &mut grid, 1);
                }
            }
        }
        // Zero-width and zero-height masks draw nothing in either painter.
        raster.width = 7;
        raster.height = 5;
        for (mask_width, mask_height) in [(0, 9), (256, 0), (0, 0)] {
            let mut glyph = Image::default();
            glyph.placement.width = mask_width;
            glyph.placement.height = mask_height;
            glyph.data = vec![255; (mask_width * mask_height) as usize];
            raster.cache.insert(('M', true, [9, 9, 9]), Some(glyph));
            let mut grid = screen(3, 3, 'M');
            for cell in &mut grid.cells {
                cell.fg = [9, 9, 9];
                cell.bg = [200, 100, 50];
                cell.bold = true;
            }
            compare_painters(&mut raster, &mut grid, 1);
        }
    }

    /// `bands_into` through a Vec, for asserting on runs directly.
    fn runs(rows: &[bool], cell_height: u32) -> Vec<DamageBand> {
        let mut out = Vec::new();
        bands_into(rows, 10, cell_height, &mut out);
        out
    }

    /// Bands copied out, so the surface is readable in the same assertion.
    fn into(
        raster: &mut Raster,
        screen: &Screen,
        dirty: &[bool],
        surface: &mut Surface,
    ) -> Vec<DamageBand> {
        raster.render_into(screen, dirty, surface).to_vec()
    }

    #[test]
    fn runs_of_dirty_rows_coalesce_into_bands() {
        assert_eq!(runs(&[false, false], 10), vec![]);
        assert_eq!(
            runs(&[true, true, false, true], 10),
            vec![
                DamageBand {
                    x: 0,
                    width: 10,
                    y: 0,
                    height: 20
                },
                DamageBand {
                    x: 0,
                    width: 10,
                    y: 30,
                    height: 10
                },
            ]
        );
        // A run that reaches the last row must still be closed.
        assert_eq!(
            runs(&[false, true, true], 4),
            vec![DamageBand {
                x: 0,
                width: 10,
                y: 4,
                height: 8
            }]
        );
        assert_eq!(
            DamageBand {
                x: 0,
                width: 10,
                y: 4,
                height: 8
            }
            .byte_range(40),
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
                x: 0,
                width: grid.cols as u32 * raster.width,
                y: 0,
                height: 3 * raster.height
            }],
            "the first render owes the whole surface"
        );

        // Poison every row, then repaint only the middle one. Anything the
        // call touches loses its poison; anything it skips keeps it.
        surface.rgba.fill(0x5a);
        let stride = surface.stride();
        for cell in &mut grid.cells[4..8] {
            cell.c = 'M';
        }
        let second = raster.render_into(&grid, &[false, true, false], &mut surface);
        assert_eq!(
            second,
            vec![DamageBand {
                x: 0,
                width: grid.cols as u32 * raster.width,
                y: raster.height,
                height: raster.height
            }]
        );
        let row = |r: u32| {
            let band = DamageBand {
                x: 0,
                width: grid.cols as u32 * raster.width,
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
        surface.invalidate(); // Losing bytes requires explicit invalidation.
        let third = raster.render_into(&grid, &[true, true, true], &mut surface);
        assert_eq!(third.len(), 1);
        // Antialiased glyph pixels can legitimately contain the poison byte.
        // Compare the complete frame, rather than forbidding a colour value.
        assert_eq!(surface.rgba(), raster.render(&grid));

        // Nothing dirty, no cursor: no work and no damage at all.
        let before = surface.rgba().to_vec();
        assert_eq!(
            raster.render_into(&grid, &[false, false, false], &mut surface),
            vec![]
        );
        assert_eq!(surface.rgba(), before);

        // A resize re-owes the whole surface even though `dirty` says nothing.
        grid = screen(4, 4, 'x');
        let resized = raster.render_into(&grid, &[false; 4], &mut surface);
        assert_eq!(
            resized,
            vec![DamageBand {
                x: 0,
                width: grid.cols as u32 * raster.width,
                y: 0,
                height: 4 * raster.height
            }]
        );
        assert_eq!(surface.grid(), (4, 4));
        assert_eq!(
            surface.rgba().len(),
            surface.stride() * surface.height() as usize
        );
    }

    #[test]
    fn the_cursor_damages_only_the_cells_it_left_and_entered() {
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
                    x: 0,
                    width: raster.width,
                    y: 0,
                    height: raster.height
                },
                DamageBand {
                    x: 0,
                    width: raster.width,
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
                x: 0,
                width: raster.width,
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
                x: 0,
                width: grid.cols as u32 * raster.width,
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
                x: 0,
                width: grid.cols as u32 * raster.width,
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
                x: 0,
                width: grid.cols as u32 * raster.width,
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
            assert!(
                bands.is_empty(),
                "{label}: reported damage it did not write"
            );
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
        let bands = raster
            .paint(&grid, &mut dst, padded, &mut state, &[])
            .to_vec();
        assert_eq!(
            bands,
            vec![DamageBand {
                x: 0,
                width: grid.cols as u32 * raster.width,
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
                vec![DamageBand {
                    x: 0,
                    width,
                    y: 0,
                    height
                }],
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
                x: 0,
                width: grid.cols as u32 * raster.width,
                y: 0,
                height: height as u32
            }],
            "a new buffer must be owed the whole frame"
        );
        assert!(!second.contains(&0x5a));
        assert_eq!(first, second);
    }

    #[test]
    fn rebind_preserves_cursor_damage_and_invalidation() {
        let mut raster = raster_with(Cursor::Block);
        let mut state = PaintState::default();
        let mut grid = screen(4, 3, 'M');
        grid.cursor_visible = true;
        grid.cursor = (1, 1);
        let stride = 4 * raster.width as usize * 4;
        let mut first = vec![0; stride * 3 * raster.height as usize];
        raster.paint(&grid, &mut first, stride, &mut state, &[]);
        let mut copied = first.clone();
        state.rebind(&copied);
        grid.cursor_visible = false;
        assert_eq!(
            raster.paint(&grid, &mut copied, stride, &mut state, &[false; 3]),
            &[DamageBand {
                x: raster.width,
                width: raster.width,
                y: raster.height,
                height: raster.height,
            }]
        );
        let mut reference = Surface::default();
        raster.render_into(&grid, &[], &mut reference);
        assert_eq!(copied, reference.rgba());

        state.invalidate();
        let mut rebound = copied.clone();
        state.rebind(&rebound);
        assert_eq!(state.grid(), (0, 0));
        assert_eq!(
            raster.paint(&grid, &mut rebound, stride, &mut state, &[false; 3]),
            &[DamageBand {
                x: 0,
                width: grid.cols as u32 * raster.width,
                y: 0,
                height: 3 * raster.height,
            }]
        );
    }

    /// The cursor is drawn by INVERTING, so it must only ever land on a row
    /// this call repainted — otherwise the second of two calls inverts an
    /// already-inverted cell and the cursor vanishes. Painting is a full
    /// overwrite of the rows it touches, so repeating a paint is a no-op.
    #[test]
    fn a_repaint_is_idempotent_and_the_cursor_only_inverts_a_painted_row() {
        // `Block` deliberately: it inverts the whole cell, so a stale cursor
        // left on an unpainted row is visible in the row comparison below. The
        // shared helper's `Underline` touches one pixel row of the cell and
        // makes the same assertions far weaker.
        let mut raster = raster_with(Cursor::Block);
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
        assert_eq!(
            row(&once, 1),
            row(&once, 0),
            "the old cursor was not erased"
        );
        assert_ne!(row(&once, 2), row(&once, 0));
    }

    /// The regression the Bevy frontend used to guard by hand, and the reason
    /// `paint` compares cols and rows SEPARATELY rather than comparing the
    /// buffer's length: 96x25 and 80x30 are both 2400 cells and therefore the
    /// same number of bytes, so a target holding one is exactly the right size
    /// to be mistaken for the other. Reusing the same buffer keeps its address
    /// and length identical too, which is what makes every cheaper check —
    /// length, identity — answer "unchanged" here.
    ///
    /// 4x3 -> 3x4 is the same trap at test scale: same bytes, different
    /// stride. Anything less than a full repaint leaves rows laid out for the
    /// old geometry on screen.
    #[test]
    fn a_shape_change_at_identical_byte_length_repaints_in_full() {
        let mut raster = raster();
        let mut state = PaintState::default();
        let wide = screen(4, 3, 'W');
        let tall = screen(3, 4, 'T');

        let (cell_w, cell_h) = (raster.width, raster.height);
        let bytes = |cols: usize, rows: usize| cols * cell_w as usize * 4 * rows * cell_h as usize;
        assert_eq!(
            bytes(4, 3),
            bytes(3, 4),
            "the trap itself is gone if these differ"
        );

        let mut buffer = vec![0_u8; bytes(4, 3)];
        let _ = raster.paint(
            &wide,
            &mut buffer,
            4 * raster.width as usize * 4,
            &mut state,
            &[],
        );

        // Same buffer, same length, nothing reported dirty. Only the recorded
        // grid shape can tell this apart from an idle frame.
        let tall_stride = 3 * raster.width as usize * 4;
        let bands = raster
            .paint(&tall, &mut buffer, tall_stride, &mut state, &[false; 4])
            .to_vec();
        assert_eq!(
            bands,
            vec![DamageBand {
                x: 0,
                width: tall.cols as u32 * raster.width,
                y: 0,
                height: 4 * raster.height
            }],
            "a shape change owes the whole frame, not the rows the VT called dirty"
        );

        // And the pixels must be the ones a cold paint of the new shape
        // produces, not a reinterpretation of the old ones.
        let mut fresh_state = PaintState::default();
        let mut fresh = vec![0_u8; bytes(3, 4)];
        let _ = raster.paint(&tall, &mut fresh, tall_stride, &mut fresh_state, &[]);
        assert_eq!(buffer, fresh);
    }

    #[test]
    fn an_empty_grid_produces_no_surface_and_no_damage() {
        let mut raster = raster();
        let mut surface = Surface::default();
        assert_eq!(
            raster.render_into(&screen(0, 0, ' '), &[], &mut surface),
            vec![]
        );
        assert!(surface.is_empty());
    }
}
