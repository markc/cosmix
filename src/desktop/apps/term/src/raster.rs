use crate::terminal::Screen;
use std::{collections::HashMap, path::PathBuf};
use swash::{
    FontRef,
    scale::{Render, ScaleContext, Source, image::Image},
    zeno::Format,
};

pub struct Raster {
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
    pub fn new(scale: f32) -> Result<Self, String> {
        let scale = scale.clamp(0.5, 8.0);
        // Logical font size (device-independent); TERM_FONT_PX overrides the
        // default. ×scale makes it physical so it stays crisp on HiDPI.
        let logical_px = std::env::var("TERM_FONT_PX")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .filter(|v| (6.0..=48.0).contains(v))
            .unwrap_or(13.0);
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
    pub fn render(&mut self, screen: &Screen) -> Vec<u8> {
        let width = screen.cols * self.width as usize;
        let height = screen.rows * self.height as usize;
        let mut rgba = vec![0; width * height * 4];
        for (i, cell) in screen.cells.iter().enumerate() {
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
        // A steady underline cursor; blinking/presentation timing is out of scope.
        let (cx, cy) = screen.cursor;
        if screen.cursor_visible && cx < screen.cols && cy < screen.rows {
            for x in cx * self.width as usize..(cx + 1) * self.width as usize {
                let offset = (((cy + 1) * self.height as usize - 1) * width + x) * 4;
                rgba[offset..offset + 4].copy_from_slice(&[220, 220, 220, 255]);
            }
        }
        rgba
    }
}
