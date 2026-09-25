//! Shaping and colour glyphs, deliberately separate from the ASCII mask loop.
use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
};
use swash::{
    CacheKey, FontRef,
    scale::{Render, ScaleContext, Source, StrikeWith, image::Content},
    shape::ShapeContext,
    text::{
        Codepoint, Script,
        cluster::{CharCluster, Parser, Status, Token},
    },
    zeno::{Format, Mask, Origin},
};

const MAX_CACHE_BYTES: usize = 16 * 1024 * 1024;
const MAX_IMAGE_PIXELS: usize = 2048 * 2048;
const MAX_LAYERS: usize = 128;

pub(super) struct Face {
    data: Arc<[u8]>,
    #[cfg(test)]
    pub(super) index: u32,
    offset: u32,
    key: CacheKey,
}

impl Face {
    fn new(data: Arc<[u8]>) -> Option<Self> {
        Self::at_index(data, 0)
    }

    fn at_index(data: Arc<[u8]>, index: u32) -> Option<Self> {
        let font = FontRef::from_index(&data, index as usize)?;
        let (offset, key) = (font.offset, font.key);
        Some(Self {
            data,
            #[cfg(test)]
            index,
            offset,
            key,
        })
    }

    pub(super) fn font(&self) -> FontRef<'_> {
        FontRef {
            data: &self.data,
            offset: self.offset,
            key: self.key,
        }
    }
}

pub(super) struct Fonts {
    pub(super) primary: Face,
    fallbacks: OnceLock<Arc<Fallbacks>>,
}

#[derive(Default)]
struct Fallbacks {
    coverage: Vec<Face>,
    emoji: Option<Face>,
    symbols: Option<Face>,
}

pub(super) const EMOJI_PATHS: &[&str] = &[
    "/usr/share/fonts/noto/NotoColorEmoji.ttf",
    "/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf",
    "/usr/share/fonts/google-noto-emoji/NotoColorEmoji.ttf",
];

impl Fonts {
    pub(super) fn discover(primary: Arc<[u8]>, index: u32) -> Option<Arc<Self>> {
        Some(Arc::new(Self {
            primary: Face::at_index(primary, index)?,
            fallbacks: OnceLock::new(),
        }))
    }

    fn fallbacks(&self) -> &Fallbacks {
        static SHARED: OnceLock<Arc<Fallbacks>> = OnceLock::new();
        fn optional(paths: &[&str]) -> Option<Face> {
            paths
                .iter()
                .find_map(|path| Face::new(std::fs::read(path).ok()?.into()))
        }
        self.fallbacks.get_or_init(|| {
            SHARED
                .get_or_init(|| {
                    Arc::new(Fallbacks {
                        coverage: super::primary_font::coverage()
                            .into_iter()
                            .filter_map(|face| Face::at_index(face.data, face.index))
                            .collect(),
                        emoji: optional(EMOJI_PATHS),
                        symbols: optional(&[
                            "/usr/share/fonts/noto/NotoSansSymbols2-Regular.ttf",
                            "/usr/share/fonts/truetype/noto/NotoSansSymbols2-Regular.ttf",
                            "/usr/share/fonts/google-noto/NotoSansSymbols2-Regular.ttf",
                        ]),
                    })
                })
                .clone()
        })
    }

    #[cfg(test)]
    pub(super) fn without_fallbacks(primary: Arc<[u8]>, index: u32) -> Arc<Self> {
        Arc::new(Self {
            primary: Face::at_index(primary, index).unwrap(),
            fallbacks: OnceLock::from(Arc::new(Fallbacks::default())),
        })
    }
}

/// Mask samples use the current foreground. Colour samples are premultiplied
/// RGBA. COLR's foreground layers remain separate masks in their original order.
pub(super) enum Pixels {
    Mask(Vec<u8>),
    Color(Vec<u8>),
}

pub(super) struct Layer {
    pub(super) x: i32,
    pub(super) y: i32,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) pixels: Pixels,
    // Native bitmap pixels can have a different ppem. Resample only in fit().
    scale: f32,
}

impl Layer {
    fn bytes(&self) -> usize {
        match &self.pixels {
            Pixels::Mask(p) | Pixels::Color(p) => p.len(),
        }
    }
}

pub(super) struct ClusterImage {
    pub(super) layers: Vec<Layer>,
    // Font selection is deterministic for the immutable font set. Keeping its
    // identity here makes cache entries font-specific without another lookup.
    font: Option<CacheKey>,
    #[cfg(test)]
    pub(super) glyphs: usize,
}

#[derive(Default)]
struct Variants([Option<ClusterImage>; 2]);

pub(super) struct UnicodeRaster {
    pub(super) fonts: Arc<Fonts>,
    shape: ShapeContext,
    scale: ScaleContext,
    cache: HashMap<Arc<str>, Variants>,
    bytes: usize,
    geometry: Option<(u32, u32, u32, i32)>,
    #[cfg(test)]
    pub(super) misses: usize,
}

impl UnicodeRaster {
    pub(super) fn new(fonts: Arc<Fonts>) -> Self {
        Self {
            fonts,
            shape: ShapeContext::new(),
            scale: ScaleContext::new(),
            cache: HashMap::new(),
            bytes: 0,
            geometry: None,
            #[cfg(test)]
            misses: 0,
        }
    }

    /// One borrowed lookup on the warm paint path, with no key allocation.
    pub(super) fn get(
        &self,
        text: &str,
        span: usize,
        px: f32,
        cell: (u32, u32),
        baseline: i32,
    ) -> Option<&ClusterImage> {
        if self.geometry != Some((px.to_bits(), cell.0, cell.1, baseline)) {
            return None;
        }
        self.cache.get(text)?.0[usize::from(span == 2)].as_ref()
    }

    pub(super) fn image(
        &mut self,
        text: &str,
        span: usize,
        px: f32,
        cell: (u32, u32),
        baseline: i32,
    ) -> &ClusterImage {
        let geometry = (px.to_bits(), cell.0, cell.1, baseline);
        if self.geometry != Some(geometry) {
            self.cache.clear();
            self.bytes = 0;
            self.geometry = Some(geometry);
        }
        let variant = usize::from(span == 2);
        if self.cache.get(text).is_none_or(|v| v.0[variant].is_none()) {
            #[cfg(test)]
            {
                self.misses += 1;
            }
            let image = self
                .rasterize(text, span, px, cell, baseline)
                .unwrap_or_else(|| tofu(cell.0 * span as u32, cell.1));
            debug_assert!(image.font.is_none_or(|key| {
                [
                    Some(&self.fonts.primary),
                    self.fonts.fallbacks().emoji.as_ref(),
                    self.fonts.fallbacks().symbols.as_ref(),
                ]
                .into_iter()
                .flatten()
                .chain(&self.fonts.fallbacks().coverage)
                .any(|face| face.key == key)
            }));
            let bytes = image.layers.iter().map(Layer::bytes).sum::<usize>();
            if self.cache.len() >= 4096 || self.bytes + bytes > MAX_CACHE_BYTES {
                self.cache.clear();
                self.bytes = 0;
            }
            self.bytes += bytes;
            self.cache.entry(Arc::from(text)).or_default().0[variant] = Some(image);
        }
        self.cache[text].0[variant].as_ref().unwrap()
    }

    fn rasterize(
        &mut self,
        text: &str,
        span: usize,
        px: f32,
        cell: (u32, u32),
        baseline: i32,
    ) -> Option<ClusterImage> {
        if text.is_empty() || text.len() > crate::clusters::MAX_CLUSTER_BYTES {
            return None;
        }
        let script = text
            .chars()
            .map(|c| c.script())
            .find(|s| !matches!(s, Script::Common | Script::Inherited))
            .unwrap_or(Script::Latin);
        let emoji = !text.contains('\u{fe0e}')
            && (text.contains('\u{fe0f}') || (span == 2 && text.chars().any(|c| c.is_emoji())));
        let primary = Some(&self.fonts.primary);
        // Only a non-ASCII cache miss reaches this path in production.
        // All rasters share these immutable bytes and stable font identities.
        let fallbacks = self.fonts.fallbacks();
        let faces = emoji
            .then_some(fallbacks.emoji.as_ref())
            .flatten()
            .into_iter()
            .chain(primary)
            .chain(&fallbacks.coverage)
            .chain((!emoji).then_some(fallbacks.emoji.as_ref()).flatten())
            .chain(fallbacks.symbols.as_ref());
        for face in faces {
            let font = face.font();
            if !covers(font, text, script) {
                continue;
            }
            let mut shaper = self.shape.builder(font).script(script).size(px).build();
            shaper.add_str(text);
            let mut glyphs = Vec::new();
            shaper.shape_with(|cluster| glyphs.extend_from_slice(cluster.glyphs));
            if glyphs.is_empty()
                || glyphs.len() > MAX_LAYERS
                || glyphs.iter().any(|g| {
                    g.id == 0 || !g.advance.is_finite() || !g.x.is_finite() || !g.y.is_finite()
                })
            {
                continue;
            }
            let mut layers = Vec::new();
            let mut pen = 0.0;
            let mut colour = false;
            let mut valid = true;
            for glyph in &glyphs {
                let x = pen + glyph.x;
                let y = baseline as f32 - glyph.y;
                pen += glyph.advance;
                match glyph_layers(&mut self.scale, font, glyph.id, px, x, y) {
                    Some((mut next, is_colour)) => {
                        colour |= is_colour;
                        layers.append(&mut next);
                    }
                    None => {
                        valid = false;
                        break;
                    }
                }
                if layers.len() > MAX_LAYERS
                    || layers.iter().map(Layer::bytes).sum::<usize>() > MAX_CACHE_BYTES / 2
                {
                    valid = false;
                    break;
                }
            }
            if !valid {
                continue;
            }
            // A space may have no mark anchor. In that case shaping places
            // its combining glyph after the space advance, outside this VT
            // cell. Keep the visible ink inside the cell without moving its
            // baseline or changing the plain ASCII painter.
            if text.starts_with(' ') && text.len() > 1 {
                layers.retain(|layer| layer.width != 0 && layer.height != 0);
                if let Some(left) = layers.iter().map(|layer| layer.x).min() {
                    let right = layers
                        .iter()
                        .map(|layer| layer.x as f32 + layer.width as f32 * layer.scale)
                        .fold(f32::MIN, f32::max)
                        .ceil() as i32;
                    let shift = (cell.0 as i32 * span as i32 - right).min(0).max(-left);
                    for layer in &mut layers {
                        layer.x += shift;
                    }
                }
            }
            // The VT, never the shaper's advance, determines the clip box.
            if (colour || emoji) && !fit(&mut layers, cell.0 * span as u32, cell.1) {
                return None;
            }
            if layers.iter().map(Layer::bytes).sum::<usize>() > MAX_CACHE_BYTES / 2 {
                return None;
            }
            return Some(ClusterImage {
                layers,
                font: Some(font.key),
                #[cfg(test)]
                glyphs: glyphs.len(),
            });
        }
        None
    }
}

fn covers(font: FontRef<'_>, text: &str, script: Script) -> bool {
    let mut parser = Parser::new(
        script,
        text.char_indices().map(|(i, ch)| Token {
            ch,
            offset: i as u32,
            len: ch.len_utf8() as u8,
            info: ch.into(),
            data: 0,
        }),
    );
    let mut cluster = CharCluster::new();
    while parser.next(&mut cluster) {
        let status = cluster.map(|ch| {
            // These participate in shaping but do not require a visible glyph.
            if matches!(ch, '\u{200c}' | '\u{200d}' | '\u{fe00}'..='\u{fe0f}'
                | '\u{e0020}'..='\u{e007f}' | '\u{e0100}'..='\u{e01ef}')
            {
                1
            } else {
                font.charmap().map(ch)
            }
        });
        if status != Status::Complete {
            return false;
        }
    }
    true
}

fn glyph_layers(
    context: &mut ScaleContext,
    font: FontRef<'_>,
    glyph: u16,
    px: f32,
    x: f32,
    y: f32,
) -> Option<(Vec<Layer>, bool)> {
    // BestFit selection, decoded at the strike's native size. Premultiply PNG
    // before our single resize; Swash's scaled bitmap path interpolates first.
    if let Some(strike) = font.color_strikes().find_by_nearest_ppem(px as u16, glyph)
        && strike.ppem() != 0
        && png_strike(font, glyph, strike.ppem())
    {
        let mut native = context.builder(font).size(f32::from(strike.ppem())).build();
        if let Some(mut image) = native.scale_color_bitmap(glyph, StrikeWith::ExactSize)
            && image.content == Content::Color
            && image.placement.width > 0
            && image.placement.height > 0
            && image.placement.width <= 2048
            && image.placement.height <= 2048
            && image.data.len() <= MAX_IMAGE_PIXELS * 4
        {
            premultiply(&mut image.data);
            let scale = px / f32::from(strike.ppem());
            let p = image.placement;
            let mut layer = Layer {
                x: 0,
                y: 0,
                width: p.width,
                height: p.height,
                pixels: Pixels::Color(image.data),
                scale,
            };
            layer.x = (x + p.left as f32 * scale).round() as i32;
            layer.y = (y - p.top as f32 * scale).round() as i32;
            return Some((vec![layer], true));
        }
    }
    let mut scaler = context.builder(font).size(px).hint(true).build();
    // Colour outlines are handled layer-by-layer, so currentColor is an alpha
    // mask and CPAL layers are premultiplied exactly once. We never reinterpret
    // Swash's already composited Content::Color as straight PNG pixels.
    if let Some(outline) = scaler.scale_color_outline(glyph) {
        if outline.len() > MAX_LAYERS {
            return None;
        }
        let palette = font.color_palettes().next();
        let mut layers = Vec::new();
        for i in 0..outline.len() {
            let layer = outline.get(i)?;
            let bounds = swash::zeno::Bounds::from_points(layer.points());
            if bounds.width() > 2048.0 || bounds.height() > 2048.0 {
                return None;
            }
            let (mask, p) = Mask::new(layer.path())
                .format(Format::Alpha)
                .origin(Origin::BottomLeft)
                .render();
            let pixels = if let Some(color) = layer
                .color_index()
                .and_then(|index| palette.map(|p| p.get(index)))
            {
                let mut data = Vec::with_capacity(mask.len() * 4);
                for a in mask {
                    let alpha = u32::from(a) * u32::from(color[3]) / 255;
                    data.extend(
                        color[..3]
                            .iter()
                            .map(|c| (u32::from(*c) * alpha / 255) as u8),
                    );
                    data.push(alpha as u8);
                }
                Pixels::Color(data)
            } else {
                Pixels::Mask(mask)
            };
            layers.push(Layer {
                x: x.round() as i32 + p.left,
                y: y.round() as i32 - p.top,
                width: p.width,
                height: p.height,
                pixels,
                scale: 1.0,
            });
        }
        return Some((layers, true));
    }
    let image = Render::new(&[Source::Outline])
        .format(Format::Alpha)
        .render(&mut scaler, glyph);
    let Some(image) = image else {
        // A space can anchor a visible combining mark. Its empty outline is
        // not a missing glyph and must not discard the other shaped glyphs.
        if glyph == font.charmap().map(' ') || glyph == font.charmap().map('\u{a0}') {
            return Some((Vec::new(), false));
        }
        return None;
    };
    if image.content != Content::Mask || image.data.len() > MAX_IMAGE_PIXELS {
        return None;
    }
    let p = image.placement;
    Some((
        vec![Layer {
            x: x.round() as i32 + p.left,
            y: y.round() as i32 - p.top,
            width: p.width,
            height: p.height,
            pixels: Pixels::Mask(image.data),
            scale: 1.0,
        }],
        false,
    ))
}

fn premultiply(data: &mut [u8]) {
    for pixel in data.chunks_exact_mut(4) {
        for c in 0..3 {
            pixel[c] = (u32::from(pixel[c]) * u32::from(pixel[3]) / 255) as u8;
        }
    }
}

/// Swash does not expose the bitmap format through Image. Accept only CBLC
/// PNG formats 17/18/19, whose decoded channels are straight RGBA. Other bitmap
/// encodings fall through to outlines/tofu rather than guessing their alpha.
fn png_strike(font: FontRef<'_>, glyph: u16, ppem: u16) -> bool {
    fn read16(data: &[u8], at: usize) -> Option<u16> {
        Some(u16::from_be_bytes(
            data.get(at..at.checked_add(2)?)?.try_into().ok()?,
        ))
    }
    fn read32(data: &[u8], at: usize) -> Option<usize> {
        Some(u32::from_be_bytes(data.get(at..at.checked_add(4)?)?.try_into().ok()?) as usize)
    }
    let check = || -> Option<bool> {
        let data = font.table(swash::tag_from_bytes(b"CBLC"))?;
        let sizes = read32(data, 4)?.min(data.len() / 48);
        for i in 0..sizes {
            let base = 8 + i * 48;
            if u16::from(*data.get(base + 45)?) != ppem {
                continue;
            }
            let array = read32(data, base)?;
            let count = read32(data, base + 8)?.min(data.len() / 8);
            for j in 0..count {
                let entry = array.checked_add(j * 8)?;
                if glyph < read16(data, entry)? || glyph > read16(data, entry + 2)? {
                    continue;
                }
                let index = array.checked_add(read32(data, entry + 4)?)?;
                return Some(matches!(read16(data, index + 2)?, 17..=19));
            }
        }
        Some(false)
    };
    check().unwrap_or(false)
}

fn fit(layers: &mut [Layer], width: u32, height: u32) -> bool {
    let Some(left) = layers.iter().map(|l| l.x).min() else {
        return true;
    };
    let top = layers.iter().map(|l| l.y).min().unwrap();
    let right = layers
        .iter()
        .map(|l| l.x as f32 + l.width as f32 * l.scale)
        .fold(f32::MIN, f32::max);
    let bottom = layers
        .iter()
        .map(|l| l.y as f32 + l.height as f32 * l.scale)
        .fold(f32::MIN, f32::max);
    let w = (right - left as f32).max(1.0);
    let h = (bottom - top as f32).max(1.0);
    let scale = (width as f32 / w).min(height as f32 / h).min(1.0);
    let dx = (width as f32 - w * scale) * 0.5;
    let dy = (height as f32 - h * scale) * 0.5;
    let size = |layer: &Layer| {
        (
            (layer.width as f32 * layer.scale * scale).round().max(1.0) as u32,
            (layer.height as f32 * layer.scale * scale).round().max(1.0) as u32,
        )
    };
    let bytes: usize = layers
        .iter()
        .map(|layer| {
            let (w, h) = size(layer);
            let channels = if matches!(layer.pixels, Pixels::Color(_)) {
                4
            } else {
                1
            };
            w as usize * h as usize * channels
        })
        .sum();
    if bytes > MAX_CACHE_BYTES / 2 {
        return false;
    }
    for layer in layers {
        layer.x = (dx + (layer.x - left) as f32 * scale).round() as i32;
        layer.y = (dy + (layer.y - top) as f32 * scale).round() as i32;
        let (w, h) = size(layer);
        resize_pixels(layer, w, h);
        layer.scale = 1.0;
    }
    true
}

/// Area sampling for reductions, bilinear for enlargement. Both operate in
/// premultiplied space, so transparent RGB cannot create a coloured fringe.
fn resize_pixels(layer: &mut Layer, width: u32, height: u32) {
    if width == layer.width && height == layer.height {
        return;
    }
    let (data, channels) = match &mut layer.pixels {
        Pixels::Mask(data) => (data, 1),
        Pixels::Color(data) => (data, 4),
    };
    if layer.width == 0 || layer.height == 0 {
        return;
    }
    let mut out = vec![0; width as usize * height as usize * channels];
    if width < layer.width || height < layer.height {
        // Integrate every covered source pixel, including fractional edge
        // coverage. A 2x2 sample aliases badly when reducing a bitmap strike.
        let scale_x = f64::from(layer.width) / f64::from(width);
        let scale_y = f64::from(layer.height) / f64::from(height);
        for y in 0..height {
            let top = f64::from(y) * scale_y;
            let bottom = f64::from(y + 1) * scale_y;
            for x in 0..width {
                let left = f64::from(x) * scale_x;
                let right = f64::from(x + 1) * scale_x;
                let mut sum = [0.0; 4];
                for sy in top.floor() as u32..(bottom.ceil() as u32).min(layer.height) {
                    let wy = bottom.min(f64::from(sy + 1)) - top.max(f64::from(sy));
                    for sx in left.floor() as u32..(right.ceil() as u32).min(layer.width) {
                        let wx = right.min(f64::from(sx + 1)) - left.max(f64::from(sx));
                        let at = (sy as usize * layer.width as usize + sx as usize) * channels;
                        for c in 0..channels {
                            sum[c] += f64::from(data[at + c]) * wx * wy;
                        }
                    }
                }
                let at = (y as usize * width as usize + x as usize) * channels;
                for c in 0..channels {
                    out[at + c] = (sum[c] / (scale_x * scale_y)).round() as u8;
                }
            }
        }
    } else {
        for y in 0..height {
            let sy = ((y as f32 + 0.5) * layer.height as f32 / height as f32 - 0.5)
                .clamp(0.0, (layer.height - 1) as f32);
            for x in 0..width {
                let sx = ((x as f32 + 0.5) * layer.width as f32 / width as f32 - 0.5)
                    .clamp(0.0, (layer.width - 1) as f32);
                let (x0, y0) = (sx as u32, sy as u32);
                let (fx, fy) = (sx - x0 as f32, sy - y0 as f32);
                let sample = |x: u32, y: u32, c: usize| {
                    f32::from(
                        data[(y.min(layer.height - 1) as usize * layer.width as usize
                            + x.min(layer.width - 1) as usize)
                            * channels
                            + c],
                    )
                };
                for c in 0..channels {
                    let a = sample(x0, y0, c) * (1.0 - fx) + sample(x0 + 1, y0, c) * fx;
                    let b = sample(x0, y0 + 1, c) * (1.0 - fx) + sample(x0 + 1, y0 + 1, c) * fx;
                    out[(y as usize * width as usize + x as usize) * channels + c] =
                        (a * (1.0 - fy) + b * fy).round() as u8;
                }
            }
        }
    }
    *data = out;
    layer.width = width;
    layer.height = height;
}

fn tofu(width: u32, height: u32) -> ClusterImage {
    let mut mask = vec![0; width as usize * height as usize];
    let (left, top) = (width / 4, height / 4);
    let (right, bottom) = (
        width.saturating_sub(left + 1),
        height.saturating_sub(top + 1),
    );
    for y in top..=bottom {
        for x in left..=right {
            if x == left || x == right || y == top || y == bottom {
                mask[(y * width + x) as usize] = 255;
            }
        }
    }
    ClusterImage {
        layers: vec![Layer {
            x: 0,
            y: 0,
            width,
            height,
            pixels: Pixels::Mask(mask),
            scale: 1.0,
        }],
        font: None,
        #[cfg(test)]
        glyphs: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raster::{PixelFormat, paint_cluster};

    #[test]
    fn sf_primary_uses_dejavu_coverage_and_keeps_ascii_and_emoji_priority() {
        use fontdb::{Database, Family, Query};
        let mut db = Database::new();
        db.load_system_fonts();
        if db
            .query(&Query {
                families: &[Family::Name("SF Mono")],
                ..Query::default()
            })
            .is_none()
        {
            eprintln!("SKIP SF coverage probe: fontdb cannot find SF Mono");
            return;
        }
        let primary = super::super::primary_font::from_database(&mut db).unwrap();
        let mut raster = crate::raster::Raster::from_font(
            primary.data,
            primary.index,
            2.5,
            crate::config::Config::default().font_px,
            crate::config::Cursor::Underline,
        )
        .unwrap();
        let fonts = raster.unicode.fonts.clone();
        let dejavu = fonts
            .fallbacks()
            .coverage
            .first()
            .expect("DejaVu coverage fixture");
        // DejaVu has no typographic-family name (ID 16); use the family (ID 1).
        let strings = dejavu.font().localized_strings();
        let family: String = strings
            .find_by_id(swash::StringId::TypographicFamily, None)
            .or_else(|| strings.find_by_id(swash::StringId::Family, None))
            .expect("coverage face family")
            .chars()
            .collect();
        assert_eq!(family, "DejaVu Sans Mono");
        for text in ["∀", "≡", "↵", "M", "0"] {
            let expected = if text.is_ascii() {
                fonts.primary.key
            } else {
                dejavu.key
            };
            if !text.is_ascii() {
                assert!(!covers(fonts.primary.font(), text, Script::Latin));
            }
            let image = raster.unicode.image(
                text,
                1,
                raster.px,
                (raster.width, raster.height),
                raster.baseline,
            );
            assert_eq!(
                image.font,
                Some(expected),
                "{text}: selected face (not tofu)"
            );
            assert!(
                image.layers.iter().any(|layer| match &layer.pixels {
                    Pixels::Mask(data) | Pixels::Color(data) => data.iter().any(|v| *v != 0),
                }),
                "{text}: non-empty ink"
            );
        }
        if let Some(emoji) = &fonts.fallbacks().emoji {
            for text in ["❤\u{fe0f}", "😀"] {
                let image = raster.unicode.image(
                    text,
                    2,
                    raster.px,
                    (raster.width, raster.height),
                    raster.baseline,
                );
                assert_eq!(image.font, Some(emoji.key), "{text}: emoji priority");
            }
        }
    }

    #[test]
    fn area_downscale_preserves_fixture_average_and_premultiplication() {
        // Thin coloured stripes alias under centre/2x2 sampling. Include
        // transparency and use a nonintegral ratio to exercise edge weights.
        let source: Vec<u8> = (0..31 * 23)
            .flat_map(|i| match i % 7 {
                0 => [240, 0, 0, 255],
                1 => [0, 100, 0, 128],
                2 => [0, 0, 60, 64],
                _ => [0, 0, 0, 0],
            })
            .collect();
        for (width, height) in [(3, 2), (1, 1), (7, 5)] {
            let mut layer = Layer {
                x: 0,
                y: 0,
                width: 31,
                height: 23,
                pixels: Pixels::Color(source.clone()),
                scale: 1.0,
            };
            resize_pixels(&mut layer, width, height);
            let Pixels::Color(result) = layer.pixels else {
                unreachable!()
            };
            for c in 0..4 {
                let mean = |pixels: &[u8]| {
                    pixels.chunks_exact(4).map(|p| f64::from(p[c])).sum::<f64>()
                        / (pixels.len() / 4) as f64
                };
                assert!((mean(&source) - mean(&result)).abs() <= 0.51, "channel {c}");
            }
            assert!(
                result
                    .chunks_exact(4)
                    .all(|p| p[..3].iter().all(|c| *c <= p[3]))
            );
        }
    }

    #[test]
    fn fallback_fonts_are_lazy_and_shared_between_rasters() {
        let mut first = crate::raster::tests::raster_with(crate::config::Cursor::Block);
        let mut second = crate::raster::tests::raster_with(crate::config::Cursor::Block);
        assert!(first.unicode.fonts.fallbacks.get().is_none());
        assert!(second.unicode.fonts.fallbacks.get().is_none());
        first.render(&crate::raster::tests::screen(8, 2, 'M'));
        assert!(
            first.unicode.fonts.fallbacks.get().is_none(),
            "ASCII must not load fallbacks"
        );
        first.render(&crate::raster::tests::screen(8, 2, 'é'));
        second.render(&crate::raster::tests::screen(8, 2, 'é'));
        assert!(Arc::ptr_eq(
            first.unicode.fonts.fallbacks.get().unwrap(),
            second.unicode.fonts.fallbacks.get().unwrap(),
        ));
    }

    #[test]
    fn straight_png_is_premultiplied_once_before_resampling() {
        // Invisible red must not bleed into opaque blue after resizing.
        let mut data = vec![255, 0, 0, 0, 0, 0, 200, 255];
        premultiply(&mut data);
        assert_eq!(data, [0, 0, 0, 0, 0, 0, 200, 255]);
        let mut layer = Layer {
            x: 0,
            y: 0,
            width: 2,
            height: 1,
            pixels: Pixels::Color(data),
            scale: 1.0,
        };
        resize_pixels(&mut layer, 1, 1);
        let Pixels::Color(data) = &layer.pixels else {
            panic!("colour");
        };
        assert_eq!(data, &[0, 0, 100, 128]);
        let image = ClusterImage {
            layers: vec![layer],
            font: None,
            glyphs: 1,
        };
        for format in [PixelFormat::Rgba, PixelFormat::Bgra] {
            let bg = format.colour([20, 40, 60]);
            let mut dst = [bg[0], bg[1], bg[2], 255, 0x5a];
            paint_cluster(&image, &mut dst, 5, 0, 0, 1, 1, [255; 3], format);
            let expected = format.colour([9, 19, 129]);
            assert_eq!(dst, [expected[0], expected[1], expected[2], 255, 0x5a]);
        }
    }

    #[test]
    fn colour_layers_and_current_foreground_masks_keep_source_over_order() {
        // Already-premultiplied red, then currentColor. This is the COLR
        // cache representation; changing foreground does not rerasterise.
        let image = ClusterImage {
            layers: vec![
                Layer {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                    pixels: Pixels::Color(vec![100, 0, 0, 128]),
                    scale: 1.0,
                },
                Layer {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                    pixels: Pixels::Mask(vec![128]),
                    scale: 1.0,
                },
            ],
            font: None,
            glyphs: 1,
        };
        for format in [PixelFormat::Rgba, PixelFormat::Bgra] {
            for fg in [[0, 200, 0], [0, 0, 200]] {
                let bg = format.colour([20, 40, 60]);
                let mut dst = [bg[0], bg[1], bg[2], 255];
                paint_cluster(&image, &mut dst, 4, 0, 0, 1, 1, fg, format);
                let first = [109_u32, 19, 29];
                let expected = format.colour(std::array::from_fn(|c| {
                    ((u32::from(fg[c]) * 128 + first[c] * 127) / 255) as u8
                }));
                assert_eq!(dst, [expected[0], expected[1], expected[2], 255]);
            }
        }
    }

    #[test]
    fn font_identity_survives_resize_and_cache_ignores_background_and_foreground() {
        let raster = crate::raster::tests::raster_with(crate::config::Cursor::Block);
        let mut resized = raster.resized(1.25, 13.0).unwrap();
        assert!(Arc::ptr_eq(&raster.unicode.fonts, &resized.unicode.fonts));
        assert_eq!(
            raster.unicode.fonts.primary.key,
            resized.unicode.fonts.primary.key
        );
        let image = resized.unicode.image(
            "e\u{301}",
            1,
            resized.px,
            (resized.width, resized.height),
            resized.baseline,
        );
        assert!(image.font.is_some());
        let misses = resized.unicode.misses;
        resized.unicode.image(
            "e\u{301}",
            1,
            resized.px,
            (resized.width, resized.height),
            resized.baseline,
        );
        assert_eq!(resized.unicode.misses, misses);
    }
}
