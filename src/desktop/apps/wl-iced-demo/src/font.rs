//! Monospace glyphs rasterised with swash at physical pixel size.

use std::collections::HashMap;
use std::path::PathBuf;
use swash::FontRef;
use swash::scale::{Render, ScaleContext, Source, image::Image};
use swash::zeno::Format;

const CANDIDATES: [&str; 7] = [
    "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/liberation/LiberationMono-Regular.ttf",
    "/usr/share/fonts/truetype/liberation2/LiberationMono-Regular.ttf",
    "/usr/share/fonts/noto/NotoSansMono-Regular.ttf",
    "/usr/share/fonts/truetype/noto/NotoSansMono-Regular.ttf",
    "/usr/share/fonts/TTF/JetBrainsMono-Regular.ttf",
];

pub struct Font {
    data: Vec<u8>,
    context: ScaleContext,
    cache: HashMap<char, Option<Image>>,
    px: f32,
    /// Cell size and baseline in physical pixels.
    pub cell_w: u32,
    pub cell_h: u32,
    pub baseline: i32,
}

impl Font {
    /// Load the first monospace font found (or `WL_DEMO_FONT`) at
    /// `logical_px * scale` physical pixels.
    pub fn load(logical_px: f32, scale: f64) -> Result<Self, String> {
        let path = match std::env::var_os("WL_DEMO_FONT") {
            Some(p) => PathBuf::from(p),
            None => CANDIDATES
                .iter()
                .map(PathBuf::from)
                .find(|p| p.is_file())
                .ok_or("no monospace font found; set WL_DEMO_FONT")?,
        };
        let data = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::from_bytes(data, logical_px * scale as f32)
    }

    pub fn from_bytes(data: Vec<u8>, px: f32) -> Result<Self, String> {
        let font = FontRef::from_index(&data, 0).ok_or("not a usable font")?;
        let metrics = font.metrics(&[]).scale(px);
        let advance = font
            .glyph_metrics(&[])
            .scale(px)
            .advance_width(font.charmap().map('M'));
        let cell_w = advance.ceil().max(1.0) as u32;
        let cell_h = (metrics.ascent + metrics.descent.abs() + metrics.leading)
            .ceil()
            .max(1.0) as u32;
        let baseline = metrics.ascent.ceil() as i32;
        Ok(Self {
            data,
            context: ScaleContext::new(),
            cache: HashMap::new(),
            px,
            cell_w,
            cell_h,
            baseline,
        })
    }

    pub fn glyph(&mut self, c: char) -> Option<&Image> {
        if !self.cache.contains_key(&c) {
            let font = FontRef::from_index(&self.data, 0)?;
            let mut scaler = self.context.builder(font).size(self.px).hint(true).build();
            let image = Render::new(&[Source::Outline])
                .format(Format::Alpha)
                .render(&mut scaler, font.charmap().map(c));
            self.cache.insert(c, image);
        }
        self.cache.get(&c)?.as_ref()
    }
}
