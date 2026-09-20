use crate::terminal::Screen;
use std::{collections::HashMap, path::PathBuf};
use swash::{
    FontRef,
    scale::{Render, ScaleContext, Source, image::Image},
    zeno::Format,
};

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
    /// Frame size in physical pixels for `screen`, and the RGBA byte length.
    pub fn frame_bytes(&self, screen: &Screen) -> usize {
        screen.cols * self.width as usize * screen.rows * self.height as usize * 4
    }
    /// Allocating form of [`Raster::render_into`] — a fresh full-frame buffer.
    /// The render path uses `render_into` against a buffer it owns across
    /// frames; this exists for callers with no buffer to keep (tests, one-shots).
    pub fn render(&mut self, screen: &Screen) -> Vec<u8> {
        let mut rgba = Vec::new();
        self.render_into(screen, &mut rgba, None);
        rgba
    }
    /// Rasterise `screen` into `dst`, reusing its allocation, and return how
    /// many rows were actually painted.
    ///
    /// `dirty` restricts the work to the rows it marks — one flag per screen
    /// row, as `Terminal::grid_snapshot` reports them. The unpainted rows keep
    /// whatever `dst` already held, so a caller passing `dirty` MUST hand back
    /// the same buffer it received the previous frame for the same geometry.
    /// Pass `None` for an unconditional full repaint.
    ///
    /// A full repaint is forced whenever `dst` is not already exactly the
    /// frame size (a resize, or a first frame), or when `dirty` does not carry
    /// one flag per row — a partial paint into a buffer of unknown provenance
    /// would leave stale pixels on screen, which is worse than the work saved.
    pub fn render_into(
        &mut self,
        screen: &Screen,
        dst: &mut Vec<u8>,
        dirty: Option<&[bool]>,
    ) -> usize {
        let width = screen.cols * self.width as usize;
        let len = self.frame_bytes(screen);
        let mut dirty = dirty.filter(|rows| rows.len() == screen.rows);
        if dst.len() != len {
            dst.clear();
            dst.resize(len, 0);
            dirty = None;
        }
        let painting = |row: usize| dirty.is_none_or(|rows| rows[row]);
        let mut painted = 0;
        for row in 0..screen.rows {
            if !painting(row) {
                continue;
            }
            painted += 1;
            let y = (row * self.height as usize) as i32;
            for col in 0..screen.cols {
                // A `Screen` whose `cells` is shorter than cols*rows is
                // malformed — `Terminal::capture` never produces one — but the
                // type is public, and the cell still gets CLEARED rather than
                // skipped: leaving the previous frame's pixels in a row this
                // call claims to have repainted is the one outcome a partial
                // repaint must never produce.
                let cell = screen.cells.get(row * screen.cols + col);
                let x = (col * self.width as usize) as i32;
                let bg = cell.map_or([0, 0, 0], |cell| cell.bg);
                let fill = [bg[0], bg[1], bg[2], 255];
                for cy in 0..self.height as usize {
                    let start = ((y as usize + cy) * width + x as usize) * 4;
                    let end = start + self.width as usize * 4;
                    for pixel in dst[start..end].chunks_exact_mut(4) {
                        pixel.copy_from_slice(&fill);
                    }
                }
                let Some(cell) = cell.filter(|cell| cell.c != ' ' && cell.c != '\0') else {
                    continue;
                };
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
                            let alpha =
                                glyph.data[(gy as u32 * p.width + gx as u32) as usize] as u32;
                            let offset = (dy as usize * width + dx as usize) * 4;
                            for channel in 0..3 {
                                dst[offset + channel] = ((cell.fg[channel] as u32 * alpha
                                    + dst[offset + channel] as u32 * (255 - alpha))
                                    / 255)
                                    as u8;
                            }
                        }
                    }
                }
            }
        }
        // Steady cursor; invert a block so its glyph remains readable. Only ever
        // drawn onto a row this call repainted: inverting a row that already
        // carries last frame's inversion would cancel it out.
        let (cx, cy) = screen.cursor;
        if screen.cursor_visible && cx < screen.cols && cy < screen.rows && painting(cy) {
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
                            for channel in &mut dst[offset..offset + 3] {
                                *channel = 255 - *channel;
                            }
                        }
                        crate::config::Cursor::Underline => {
                            dst[offset..offset + 4].copy_from_slice(&[220, 220, 220, 255]);
                        }
                    }
                }
            }
        }
        painted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{Cell, Screen};
    use std::time::Instant;

    fn raster() -> Raster {
        Raster::new(1.0, 13.0, crate::config::Cursor::Block).unwrap()
    }

    fn screen(cols: usize, rows: usize) -> Screen {
        Screen {
            cols,
            rows,
            cursor: (0, 0),
            cursor_visible: false,
            cells: (0..cols * rows)
                .map(|i| Cell {
                    c: char::from(b'a' + (i % 26) as u8),
                    fg: [200, 200, 200],
                    bg: [10, 20, 30],
                    bold: false,
                })
                .collect(),
            updated: Instant::now(),
        }
    }

    /// The property the whole damage-rect path rests on: a partial repaint into
    /// the buffer that holds the previous frame is byte-identical to repainting
    /// the whole screen. If this is ever false, the screen shows stale pixels.
    #[test]
    fn a_partial_repaint_of_the_changed_rows_matches_a_full_one() {
        let mut painter = raster();
        let before = screen(20, 6);
        let mut buffer = painter.render(&before);
        let mut after = screen(20, 6);
        after.cells[3 * 20 + 7].c = 'Z';
        after.cells[3 * 20 + 7].bg = [90, 0, 0];
        let mut dirty = vec![false; 6];
        dirty[3] = true;
        let painted = painter.render_into(&after, &mut buffer, Some(&dirty));
        assert_eq!(painted, 1, "only the changed row is rasterised");
        assert_eq!(buffer, painter.render(&after), "partial == full");
    }

    /// Same property with the cursor moving between two rows: the row it left
    /// must lose its inversion, and the row it arrived on must gain one.
    #[test]
    fn a_moved_cursor_repaints_to_the_same_pixels_as_a_full_frame() {
        let mut painter = raster();
        let mut before = screen(12, 4);
        before.cursor = (2, 1);
        before.cursor_visible = true;
        let mut buffer = painter.render(&before);
        let mut after = screen(12, 4);
        after.cursor = (5, 2);
        after.cursor_visible = true;
        // What `refresh` marks: the row the cursor left and the row it entered.
        let mut dirty = vec![false; 4];
        dirty[1] = true;
        dirty[2] = true;
        assert_eq!(painter.render_into(&after, &mut buffer, Some(&dirty)), 2);
        assert_eq!(buffer, painter.render(&after));
    }

    /// A row that is not repainted must not have the cursor drawn onto it
    /// again — a second inversion of the same pixels cancels the first out.
    #[test]
    fn an_unpainted_cursor_row_is_left_exactly_as_it_was() {
        let mut painter = raster();
        let mut frame = screen(8, 3);
        frame.cursor = (1, 1);
        frame.cursor_visible = true;
        let mut buffer = painter.render(&frame);
        let settled = buffer.clone();
        assert_eq!(painter.render_into(&frame, &mut buffer, Some(&[false; 3])), 0);
        assert_eq!(buffer, settled, "a frame with no dirty rows changes nothing");
    }

    /// The two ways a caller can hand over a buffer that cannot be trusted to
    /// hold the previous frame. Both must repaint in full rather than leave
    /// whatever was there: a fresh/resized buffer, and a dirty slice that does
    /// not describe this screen.
    #[test]
    fn an_untrustworthy_buffer_or_dirty_slice_repaints_in_full() {
        let mut painter = raster();
        let frame = screen(9, 5);
        let full = painter.render(&frame);

        let mut fresh = Vec::new();
        assert_eq!(painter.render_into(&frame, &mut fresh, Some(&[false; 5])), 5);
        assert_eq!(fresh, full, "an empty buffer ignores the dirty slice");

        let mut resized = vec![0xAB; 4];
        assert_eq!(painter.render_into(&frame, &mut resized, Some(&[false; 5])), 5);
        assert_eq!(resized, full, "a wrong-sized buffer ignores the dirty slice");

        let mut wrong = painter.render(&frame);
        wrong[0] = 0xAB;
        assert_eq!(painter.render_into(&frame, &mut wrong, Some(&[false; 2])), 5);
        assert_eq!(wrong, full, "a dirty slice of the wrong length repaints all");
    }

    /// `render_into` reuses the caller's allocation. The old render path built
    /// a fresh full-frame `Vec` (~12 MB at 2.5x) for every damaged frame, which
    /// is the allocation D5 removes.
    /// `Screen` is public, so a caller can hand over fewer cells than the
    /// dimensions claim. A repainted row must still come out CLEAN: the one
    /// thing a partial repaint must never do is leave the previous frame's
    /// pixels in a row it reported as painted.
    #[test]
    fn a_short_cells_vector_still_clears_every_cell_of_a_painted_row() {
        let mut painter = raster();
        let busy = screen(6, 2);
        let mut buffer = painter.render(&busy);
        let mut truncated = screen(6, 2);
        truncated.cells.truncate(8);
        truncated.cursor = (5, 1);
        truncated.cursor_visible = true;
        painter.render_into(&truncated, &mut buffer, None);
        assert_eq!(buffer, painter.render(&truncated), "no pixel survives");
        // And the cursor's inversion is not applied twice to the same pixels.
        let once = buffer.clone();
        painter.render_into(&truncated, &mut buffer, None);
        assert_eq!(buffer, once, "a repaint is idempotent, inversion included");
    }

    #[test]
    fn a_same_size_frame_reuses_the_callers_allocation() {
        let mut painter = raster();
        let frame = screen(14, 7);
        let mut buffer = painter.render(&frame);
        let address = buffer.as_ptr();
        painter.render_into(&frame, &mut buffer, None);
        assert_eq!(buffer.as_ptr(), address, "no reallocation for a same-size frame");
    }
}
