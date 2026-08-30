//! Waveform level-of-detail pyramid + lane/ruler painters for arranger-style
//! timeline views.
//!
//! The pyramid is a mono port of the min/max mipmap from `~/.gh/icedaw`
//! (`icedaw_gui/src/lod.rs`, MIT): level 0 folds 8 frames per slot, every
//! deeper level folds 8 slots of the previous one, and `update` rebuilds
//! incrementally from a start frame so live-recording appends stay cheap.
//! Painting picks the deepest level whose slot still fits inside one pixel
//! column, so a repaint costs O(width × ≤8) folds at ANY zoom — never a
//! rescan of the samples, which the UI does not keep.

use bevy::asset::RenderAssetUsages;
use bevy::color::{Color, ColorToComponents as _};
use bevy::image::Image;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};

/// log2 of the fold factor between levels (8 frames/slots per slot).
const STEP_SIZE: usize = 3;
const CHUNK_SIZE: usize = 1 << STEP_SIZE;

/// Min/max mipmap pyramid over a mono signal. Holds only the pyramid
/// (~samples/4 bytes), not the samples — displayable zoom bottoms out at
/// level 0's 8 frames per pixel column, far past arranger territory.
#[derive(Clone, Debug, Default)]
pub struct WavePyramid {
    levels: Vec<Vec<(f32, f32)>>,
    len_frames: usize,
}

impl WavePyramid {
    #[must_use]
    pub fn new(samples: &[f32]) -> Self {
        let mut pyramid = Self::default();
        pyramid.update(samples, 0);
        pyramid
    }

    /// Frames the pyramid was built over.
    #[must_use]
    pub fn len_frames(&self) -> usize {
        self.len_frames
    }

    /// Rebuild the pyramid from `start` onward: slots wholly before `start`
    /// are kept, everything at/after is recomputed from `samples` (the full
    /// signal — `start` only bounds the recompute). A `start` past the end of
    /// a shrunken signal is clamped so stale tail slots never survive.
    pub fn update(&mut self, samples: &[f32], start: usize) {
        self.len_frames = samples.len();
        let mut start = start.min(samples.len()) / CHUNK_SIZE;

        if self.levels.is_empty() {
            self.levels.push(Vec::new());
        }

        self.levels[0].truncate(start);
        self.levels[0].extend(
            samples[(CHUNK_SIZE * start).min(samples.len())..]
                .chunks(CHUNK_SIZE)
                .map(samples_min_max),
        );

        for i in 1.. {
            if self.levels[i - 1].len() < CHUNK_SIZE {
                self.levels.truncate(i);
                return;
            }

            if self.levels.len() == i {
                self.levels.push(Vec::new());
            }

            let [last, current] = &mut self.levels[i - 1..=i] else {
                unreachable!();
            };

            start /= CHUNK_SIZE;
            current.truncate(start);
            current.extend(
                last[(CHUNK_SIZE * start).min(last.len())..]
                    .chunks(CHUNK_SIZE)
                    .map(lod_min_max),
            );
        }
    }

    /// Largest absolute sample value seen (from the coarsest level).
    #[must_use]
    pub fn max_abs(&self) -> f32 {
        self.levels.last().map_or(0.0, |level| {
            level
                .iter()
                .fold(0.0, |acc, &(min, max)| min.abs().max(max.abs()).max(acc))
        })
    }

    /// One `(min, max)` per pixel column for a viewport starting at
    /// `start_frame` with `frames_per_px` frames per column. Columns past the
    /// end of the signal are `None`. `frames_per_px` below `CHUNK_SIZE` is
    /// served from level 0 (over-smoothed but correct); callers should clamp
    /// their zoom there anyway.
    #[must_use]
    pub fn columns(
        &self,
        start_frame: f64,
        frames_per_px: f64,
        width: usize,
    ) -> Vec<Option<(f32, f32)>> {
        let Some(level0) = self.levels.first() else {
            return vec![None; width];
        };
        if frames_per_px <= 0.0 {
            return vec![None; width];
        }

        // Deepest level whose slot span still fits in one column.
        let mut level_idx = 0;
        while level_idx + 1 < self.levels.len()
            && slot_frames(level_idx + 1) as f64 <= frames_per_px
        {
            level_idx += 1;
        }
        let level = if level_idx == 0 {
            level0
        } else {
            &self.levels[level_idx]
        };
        let slot = slot_frames(level_idx) as f64;

        (0..width)
            .map(|x| {
                let f0 = start_frame + x as f64 * frames_per_px;
                let f1 = f0 + frames_per_px;
                if f1 <= 0.0 || f0 >= self.len_frames as f64 {
                    return None;
                }
                let s0 = (f0.max(0.0) / slot) as usize;
                let s1 = ((f1 / slot).ceil() as usize).min(level.len()).max(s0 + 1);
                if s0 >= level.len() {
                    return None;
                }
                Some(lod_min_max(&level[s0..s1.min(level.len())]))
            })
            .collect()
    }
}

/// Frames covered by one slot of level `idx`.
fn slot_frames(idx: usize) -> usize {
    CHUNK_SIZE << (STEP_SIZE * idx)
}

fn samples_min_max(chunk: &[f32]) -> (f32, f32) {
    chunk
        .iter()
        .fold((f32::INFINITY, f32::NEG_INFINITY), |(min, max), &c| {
            (min.min(c), max.max(c))
        })
}

fn lod_min_max(chunk: &[(f32, f32)]) -> (f32, f32) {
    chunk
        .iter()
        .fold((f32::INFINITY, f32::NEG_INFINITY), |(min, max), &c| {
            (min.min(c.0), max.max(c.1))
        })
}

/// Lane background; matches the studio waves-lane palette.
pub const LANE_BG: [u8; 4] = [23, 23, 28, 255];
/// The dim centre (zero) line drawn through each lane.
pub const LANE_CENTRE: [u8; 4] = [255, 255, 255, 30];

/// One non-destructive region for DISPLAY: a window of the lane's source
/// placed on the timeline, mirroring the engine's region model (frames,
/// 1:1, linear fades). Framework-light on purpose — apps convert their
/// engine's region type into this, so ctk never depends on the engine crate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WaveRegion {
    /// First timeline frame the region occupies.
    pub timeline_start: u64,
    /// First source frame the region reads (the pyramid is source-indexed).
    pub source_start: u64,
    /// Region length in frames.
    pub len: u64,
    /// Linear gain multiplier (scales the drawn amplitude).
    pub gain: f32,
    /// Linear fade-in / fade-out lengths in frames.
    pub fade_in: u64,
    pub fade_out: u64,
    /// Draw the selected-region highlight.
    pub selected: bool,
}

impl WaveRegion {
    /// One past the region's last timeline frame.
    pub fn timeline_end(&self) -> u64 {
        self.timeline_start.saturating_add(self.len)
    }

    /// The display envelope at a region-local position — gain × linear
    /// fades (multiplied when they overlap), matching the engine's mix so
    /// what is drawn is what plays.
    pub fn envelope(&self, pos: u64) -> f32 {
        if pos >= self.len {
            return 0.0;
        }
        let mut mix = self.gain;
        if self.fade_in > 0 && pos < self.fade_in {
            mix *= pos as f32 / self.fade_in as f32;
        }
        if self.fade_out > 0 {
            let remaining = self.len - 1 - pos;
            if remaining < self.fade_out {
                mix *= remaining as f32 / self.fade_out as f32;
            }
        }
        mix
    }
}

/// Draw one waveform column (min/max span mapped [-1,1] → rows, ≥1 px tall
/// so silence reads as a hairline on the centre).
fn draw_column(data: &mut [u8], width: u32, height: u32, x: u32, lo: f32, hi: f32, rgb: [u8; 3]) {
    let y_top = ((0.5 - hi.clamp(-1.0, 1.0) * 0.5) * (height - 1) as f32) as u32;
    let y_bot = (((0.5 - lo.clamp(-1.0, 1.0) * 0.5) * (height - 1) as f32) as u32).max(y_top);
    for y in y_top..=y_bot.min(height - 1) {
        let o = ((y * width + x) * 4) as usize;
        data[o] = rgb[0];
        data[o + 1] = rgb[1];
        data[o + 2] = rgb[2];
        data[o + 3] = 255;
    }
}

/// The opaque RGBA bytes of a theme colour — the lane/ruler background the
/// paint fns fill with (was the hardcoded [`LANE_BG`]; now theme-driven).
pub fn rgba_bytes(color: Color) -> [u8; 4] {
    let [r, g, b] = color_bytes(color);
    [r, g, b, 255]
}

fn lane_background(width: u32, height: u32, bg: [u8; 4]) -> Vec<u8> {
    let mut data = vec![0u8; (width * height * 4) as usize];
    for (i, px) in data.chunks_exact_mut(4).enumerate() {
        let y = i as u32 / width;
        px.copy_from_slice(if y == height / 2 { &LANE_CENTRE } else { &bg });
    }
    data
}

/// Paint one lane's waveform into an RGBA texture: dark background, dim
/// centre line, the colour's min/max span per column. Adjacent columns are
/// forced to overlap (the icedaw continuity scan) so scrolling waveforms
/// never show vertical gaps, and silence keeps a 1-px hairline on the centre.
#[must_use]
pub fn paint_wave_lane(
    pyramid: &WavePyramid,
    start_frame: f64,
    frames_per_px: f64,
    width: u32,
    height: u32,
    color: Color,
    bg: Color,
) -> Image {
    let width = width.max(1);
    let height = height.max(2);
    let rgb = color_bytes(color);
    let mut data = lane_background(width, height, rgba_bytes(bg));

    let columns = pyramid.columns(start_frame, frames_per_px, width as usize);
    let mut prev: Option<(f32, f32)> = None;
    for (x, column) in columns.into_iter().enumerate() {
        let Some((lo, hi)) = column else {
            prev = None;
            continue;
        };
        // Overlap with the previous column so the outline stays connected.
        let (lo, hi) = prev.map_or((lo, hi), |(p_lo, p_hi)| (lo.min(p_hi), hi.max(p_lo)));
        prev = Some((lo, hi));
        draw_column(&mut data, width, height, x as u32, lo, hi, rgb);
    }

    rgba_image(width, height, data)
}

/// Paint one lane's REGIONS into an RGBA texture: each region draws its
/// pyramid slice (source-offset window placed at its timeline position)
/// scaled by the gain/fade envelope, over a subtle colour tint spanning the
/// region; 1-px edge borders mark the boundaries and a selected region gets
/// a brighter tint + top/bottom rails. Timeline gaps stay plain background —
/// what is drawn is exactly what the engine plays.
pub struct RegionLanePaintParams<'a> {
    pub pyramid: &'a WavePyramid,
    pub regions: &'a [WaveRegion],
    pub start_frame: f64,
    pub frames_per_px: f64,
    pub width: u32,
    pub height: u32,
    pub color: Color,
    pub background: Color,
}

#[must_use]
pub fn paint_region_lane(params: RegionLanePaintParams<'_>) -> Image {
    let RegionLanePaintParams {
        pyramid,
        regions,
        start_frame,
        frames_per_px,
        width,
        height,
        color,
        background,
    } = params;
    let width = width.max(1);
    let height = height.max(2);
    let rgb = color_bytes(color);
    let bg = rgba_bytes(background);
    let mut data = lane_background(width, height, bg);

    for region in regions {
        if frames_per_px <= 0.0 || region.len == 0 {
            continue;
        }
        // Visible column span of this region.
        let x0 = ((region.timeline_start as f64 - start_frame) / frames_per_px).floor();
        let x1 = ((region.timeline_end() as f64 - start_frame) / frames_per_px).ceil();
        if x1 <= 0.0 || x0 >= f64::from(width) {
            continue;
        }
        let col0 = x0.max(0.0) as u32;
        let col1 = (x1.min(f64::from(width)) as u32).max(col0 + 1).min(width);

        // Region body tint (selected = brighter) + selection rails.
        let tint: u8 = if region.selected { 46 } else { 22 };
        for x in col0..col1 {
            for y in 0..height {
                let o = ((y * width + x) * 4) as usize;
                let is_rail = region.selected && (y == 0 || y == height - 1);
                if is_rail {
                    data[o..o + 4].copy_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
                } else if y != height / 2 {
                    // Colour-scaled tint over the lane background.
                    for c in 0..3 {
                        let add = (u16::from(rgb[c]) * u16::from(tint) / 255) as u8;
                        data[o + c] = bg[c].saturating_add(add);
                    }
                }
            }
        }

        // The waveform: pyramid columns from the region's SOURCE window,
        // mapped on the COLUMN GRID — column x covers timeline
        // [start + x·fpp, +fpp), so its source read starts at
        // source_start + (that timeline − region start). Deriving from the
        // region's own start instead would carry a sub-column offset through
        // the whole region (and let the tail read past the source window).
        // The head clamp makes a partial first column show the region's
        // head; the tail is split off as a width-clipped single-column query
        // so no column ever reads source past the region's window.
        // Head/tail edge columns are quantised to whole LOD slots by
        // `columns()` — each edge may fold in up to one selected-LOD slot
        // (so up to ~a display column per SIDE) from beyond the region
        // window. The pyramid's inherent display resolution; accepted.
        let col0_timeline = start_frame + f64::from(col0) * frames_per_px;
        let src_hi = region.source_start.saturating_add(region.len) as f64;
        let n_cols = (col1 - col0) as usize;
        // Partial FIRST column (region starts mid-column): its own clipped
        // query over just the covered span, so the bulk grid after it stays
        // exactly column-aligned instead of inheriting a sub-column offset.
        let head_span = (col0_timeline + frames_per_px) - region.timeline_start as f64;
        let head_partial = col0_timeline < region.timeline_start as f64;
        let mut columns = Vec::with_capacity(n_cols);
        let mut src_lo = if head_partial {
            let covered = head_span.min(region.len as f64);
            if covered > 0.0 {
                columns.extend(pyramid.columns(region.source_start as f64, covered, 1));
            } else {
                columns.push(None);
            }
            region.source_start as f64 + covered
        } else {
            region.source_start as f64 + (col0_timeline - region.timeline_start as f64)
        };
        src_lo = src_lo.min(src_hi);
        let bulk_cols = n_cols - columns.len().min(n_cols);
        let full_cols = (((src_hi - src_lo) / frames_per_px).floor() as usize).min(bulk_cols);
        columns.extend(pyramid.columns(src_lo, frames_per_px, full_cols));
        let tail_span = src_hi - (src_lo + full_cols as f64 * frames_per_px);
        if columns.len() < n_cols && tail_span > 0.0 {
            columns.extend(pyramid.columns(
                src_lo + full_cols as f64 * frames_per_px,
                tail_span,
                1,
            ));
        }
        let mut prev: Option<(f32, f32)> = None;
        for (i, column) in columns.into_iter().enumerate() {
            let x = col0 + i as u32;
            // Envelope at the centre of the column's COVERED span (the
            // intersection of column and region): a partially covered edge
            // column evaluates the frames it actually shows, instead of a
            // full-column centre that may land outside the region (or, for
            // a fade tail, clamp onto the silent last frame). Region-local
            // arithmetic — no absolute-f64 clamp to misbehave at extremes.
            let col_t0 = col0_timeline + i as f64 * frames_per_px;
            let covered_lo = col_t0.max(region.timeline_start as f64);
            let covered_hi = (col_t0 + frames_per_px).min(region.timeline_end() as f64);
            let centre = (covered_lo + covered_hi) * 0.5;
            let pos = (centre - region.timeline_start as f64).max(0.0) as u64;
            let mix = region.envelope(pos.min(region.len.saturating_sub(1)));
            let Some((lo, hi)) = column else {
                prev = None;
                continue;
            };
            let (lo, hi) = (lo * mix, hi * mix);
            let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
            let (lo, hi) = prev.map_or((lo, hi), |(p_lo, p_hi)| (lo.min(p_hi), hi.max(p_lo)));
            prev = Some((lo, hi));
            draw_column(&mut data, width, height, x, lo, hi, rgb);
        }

        // Edge borders: full-height 1-px columns — only where the boundary
        // is actually VISIBLE (a region running past the viewport edge must
        // not grow a false boundary at the clip).
        let mut borders = Vec::with_capacity(2);
        if x0 >= 0.0 {
            borders.push(col0);
        }
        if x1 <= f64::from(width) {
            borders.push(col1 - 1);
        }
        for x in borders {
            for y in 0..height {
                let o = ((y * width + x) * 4) as usize;
                data[o..o + 4].copy_from_slice(&[
                    rgb[0].saturating_add(40),
                    rgb[1].saturating_add(40),
                    rgb[2].saturating_add(40),
                    255,
                ]);
            }
        }
    }

    rgba_image(width, height, data)
}

/// Tick positions for a time ruler over a viewport. `major` carries the pixel
/// x and the tick's time in seconds (the caller renders labels — text can't
/// go into the tick texture); `minor` is bare pixel positions. The major step
/// is the smallest of a musical-feeling ladder that keeps labels ≥ ~70 px
/// apart; minors subdivide each major span.
#[derive(Clone, Debug, Default)]
pub struct RulerTicks {
    pub major: Vec<(f32, f64)>,
    pub minor: Vec<f32>,
    pub major_step_secs: f64,
}

/// The major tick step + subdivision count for a zoom level — the ladder
/// both the ruler and the snap quantum derive from.
fn ruler_major_step(secs_per_px: f64) -> (f64, u32) {
    const STEPS: [(f64, u32); 9] = [
        (0.1, 5),
        (0.5, 5),
        (1.0, 5),
        (2.0, 4),
        (5.0, 5),
        (10.0, 5),
        (30.0, 6),
        (60.0, 6),
        (300.0, 5),
    ];
    const MIN_MAJOR_PX: f64 = 70.0;
    STEPS
        .into_iter()
        .find(|(step, _)| step / secs_per_px >= MIN_MAJOR_PX)
        .unwrap_or((600.0, 5))
}

/// The MINOR tick step at this zoom — what edit gestures snap to, so
/// snapping always lands on a line the user can see.
#[must_use]
pub fn ruler_minor_step_secs(secs_per_px: f64) -> f64 {
    let (step, subdivisions) = ruler_major_step(secs_per_px);
    step / f64::from(subdivisions)
}

#[must_use]
pub fn ruler_ticks(start_secs: f64, secs_per_px: f64, width: u32) -> RulerTicks {
    if secs_per_px <= 0.0 || width == 0 {
        return RulerTicks::default();
    }
    let (step, subdivisions) = ruler_major_step(secs_per_px);

    let end_secs = start_secs + f64::from(width) * secs_per_px;
    let minor_step = step / f64::from(subdivisions);
    let mut ticks = RulerTicks {
        major_step_secs: step,
        ..Default::default()
    };
    let first = (start_secs / minor_step).ceil().max(0.0) as u64;
    for n in first.. {
        let t = n as f64 * minor_step;
        if t > end_secs {
            break;
        }
        let x = ((t - start_secs) / secs_per_px) as f32;
        // Guard the float seam: n*minor_step may land a hair under start.
        if x < 0.0 || x >= width as f32 {
            continue;
        }
        if n % u64::from(subdivisions) == 0 {
            ticks.major.push((x, t));
        } else {
            ticks.minor.push(x);
        }
    }
    ticks
}

/// Paint the ruler's tick texture (labels are the caller's Text nodes).
#[must_use]
pub fn paint_ruler(ticks: &RulerTicks, width: u32, height: u32, color: Color, bg: Color) -> Image {
    let width = width.max(1);
    let height = height.max(4);
    let [cr, cg, cb] = color_bytes(color);
    let bg = rgba_bytes(bg);
    let mut data = vec![0u8; (width * height * 4) as usize];
    for px in data.chunks_exact_mut(4) {
        px.copy_from_slice(&bg);
    }
    let mut mark = |x: f32, from_y: u32, alpha: u8| {
        let x = x as u32;
        if x >= width {
            return;
        }
        for y in from_y..height {
            let o = ((y * width + x) * 4) as usize;
            data[o] = cr;
            data[o + 1] = cg;
            data[o + 2] = cb;
            data[o + 3] = alpha;
        }
    };
    for &x in &ticks.minor {
        mark(x, height - height / 3, 120);
    }
    for &(x, _) in &ticks.major {
        mark(x, height / 3, 255);
    }
    rgba_image(width, height, data)
}

/// `M:SS` (or `M:SS.d` for sub-second steps) — ruler label formatting.
#[must_use]
pub fn format_ruler_secs(t: f64, step_secs: f64) -> String {
    let minutes = (t / 60.0) as u64;
    let seconds = t - minutes as f64 * 60.0;
    if step_secs < 1.0 {
        format!("{minutes}:{seconds:04.1}")
    } else {
        format!("{minutes}:{:02}", seconds.round() as u64)
    }
}

fn color_bytes(color: Color) -> [u8; 3] {
    let [r, g, b, _] = color.to_srgba().to_f32_array();
    // Round, don't truncate: 1.0 can round-trip through sRGB as 0.9999…,
    // which `as u8` would floor to 254.
    [
        (r * 255.0).round() as u8,
        (g * 255.0).round() as u8,
        (b * 255.0).round() as u8,
    ]
}

fn rgba_image(width: u32, height: u32, data: Vec<u8>) -> Image {
    Image::new(
        Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        data,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(n: usize) -> Vec<f32> {
        (0..n).map(|i| (i as f32 / n as f32) * 2.0 - 1.0).collect()
    }

    #[test]
    fn level0_matches_naive_min_max() {
        let samples = ramp(1000);
        let pyramid = WavePyramid::new(&samples);
        for (slot, chunk) in pyramid.levels[0].iter().zip(samples.chunks(CHUNK_SIZE)) {
            let naive = samples_min_max(chunk);
            assert_eq!(*slot, naive);
        }
    }

    #[test]
    fn deeper_levels_fold_consistently() {
        let samples: Vec<f32> = (0..40_000)
            .map(|i| ((i * 37) % 101) as f32 / 50.0 - 1.0)
            .collect();
        let pyramid = WavePyramid::new(&samples);
        assert!(pyramid.levels.len() >= 3);
        for i in 1..pyramid.levels.len() {
            for (slot, chunk) in pyramid.levels[i]
                .iter()
                .zip(pyramid.levels[i - 1].chunks(CHUNK_SIZE))
            {
                assert_eq!(*slot, lod_min_max(chunk));
            }
        }
        // Every level's global min/max agrees with the signal's.
        let (lo, hi) = samples_min_max(&samples);
        for level in &pyramid.levels {
            let folded = lod_min_max(level);
            assert_eq!(folded, (lo, hi));
        }
    }

    #[test]
    fn incremental_update_equals_full_rebuild() {
        let mut samples: Vec<f32> = (0..10_000).map(|i| (i as f32 * 0.7).sin()).collect();
        let mut incremental = WavePyramid::new(&samples);
        // Mutate a tail region and append, then update from the edit point.
        let edit_at = 7_777;
        for s in &mut samples[edit_at..] {
            *s = -*s;
        }
        samples.extend((0..3_000).map(|i| (i as f32 * 0.31).cos()));
        incremental.update(&samples, edit_at);
        let full = WavePyramid::new(&samples);
        assert_eq!(incremental.levels, full.levels);
        assert_eq!(incremental.len_frames(), full.len_frames());
    }

    #[test]
    fn shrinking_update_drops_stale_tail() {
        let samples = ramp(50_000);
        let mut pyramid = WavePyramid::new(&samples);
        let short = &samples[..600];
        pyramid.update(short, 0);
        assert_eq!(pyramid.levels, WavePyramid::new(short).levels);
    }

    #[test]
    fn shrinking_update_with_stale_start_clamps() {
        // `start` pointing past the end of the shrunken signal (e.g. an edit
        // that truncated audio) must not leave stale tail slots behind.
        let samples = ramp(50_000);
        let mut pyramid = WavePyramid::new(&samples);
        let short = &samples[..600];
        pyramid.update(short, 40_000);
        assert_eq!(pyramid.levels, WavePyramid::new(short).levels);
        assert_eq!(pyramid.len_frames(), 600);
    }

    #[test]
    fn columns_cover_signal_and_pick_sane_bounds() {
        let n = 48_000 * 30;
        let samples: Vec<f32> = (0..n).map(|i| (i as f32 * 0.01).sin()).collect();
        let pyramid = WavePyramid::new(&samples);
        for fpp in [8.0, 64.0, 1000.0, 30_000.0] {
            let width = 500;
            let columns = pyramid.columns(0.0, fpp, width);
            assert_eq!(columns.len(), width);
            let covered = ((n as f64) / fpp).ceil() as usize;
            for (x, column) in columns.iter().enumerate() {
                if x < covered.min(width) {
                    let (lo, hi) = column.expect("in-signal column");
                    assert!((-1.0..=1.0).contains(&lo) && lo <= hi && hi <= 1.0);
                } else {
                    assert!(column.is_none(), "column {x} past the end must be None");
                }
            }
        }
    }

    #[test]
    fn columns_min_max_envelope_contains_naive() {
        // The chosen LOD's slots straddle column edges, so each column's span
        // must CONTAIN (not equal) the naive per-column min/max.
        let samples: Vec<f32> = (0..100_000)
            .map(|i| (i as f32 * 0.037).sin() * 0.9)
            .collect();
        let pyramid = WavePyramid::new(&samples);
        let fpp = 517.3;
        let columns = pyramid.columns(0.0, fpp, 150);
        for (x, column) in columns.iter().enumerate() {
            let f0 = (x as f64 * fpp) as usize;
            let f1 = (((x + 1) as f64) * fpp) as usize;
            if f0 >= samples.len() {
                break;
            }
            let (n_lo, n_hi) = samples_min_max(&samples[f0..f1.min(samples.len())]);
            let (lo, hi) = column.expect("in-signal column");
            assert!(lo <= n_lo + 1e-6 && hi >= n_hi - 1e-6);
        }
    }

    #[test]
    fn empty_and_zero_zoom_are_silent() {
        let pyramid = WavePyramid::new(&[]);
        assert_eq!(pyramid.columns(0.0, 100.0, 8), vec![None; 8]);
        let pyramid = WavePyramid::new(&ramp(100));
        assert_eq!(pyramid.columns(0.0, 0.0, 4), vec![None; 4]);
        assert!(pyramid.max_abs() > 0.9 && pyramid.max_abs() <= 1.0);
    }

    #[test]
    fn region_envelope_matches_engine_semantics() {
        let region = WaveRegion {
            timeline_start: 0,
            source_start: 0,
            len: 100,
            gain: 0.5,
            fade_in: 10,
            fade_out: 20,
            selected: false,
        };
        assert_eq!(region.envelope(0), 0.0);
        assert_eq!(region.envelope(5), 0.25);
        assert_eq!(region.envelope(50), 0.5);
        assert_eq!(region.envelope(89), 0.25);
        assert_eq!(region.envelope(99), 0.0);
        assert_eq!(region.envelope(100), 0.0);
    }

    #[test]
    fn region_paint_maps_source_window_to_timeline_position() {
        // Source: silence except a loud burst at frames 800..1000. A region
        // reading that burst (source_start 800) placed at timeline 100 must
        // paint amplitude ONLY inside its timeline span.
        let mut samples = vec![0.0f32; 2000];
        for (i, s) in samples[800..1000].iter_mut().enumerate() {
            // Bipolar burst so each column spans the full lane height (a
            // constant signal would paint min==max: a 1px line).
            *s = if i % 2 == 0 { 1.0 } else { -1.0 };
        }
        let pyramid = WavePyramid::new(&samples);
        let region = WaveRegion {
            timeline_start: 100,
            source_start: 800,
            len: 200,
            gain: 1.0,
            fade_in: 0,
            fade_out: 0,
            selected: false,
        };
        // fpp 10: region spans columns 10..30 of a 50-wide viewport at 0.
        let image = paint_region_lane(RegionLanePaintParams {
            pyramid: &pyramid,
            regions: &[region],
            start_frame: 0.0,
            frames_per_px: 10.0,
            width: 50,
            height: 40,
            color: Color::WHITE,
            background: Color::srgb_u8(23, 23, 28),
        });
        let data = image.data.as_ref().expect("cpu image data");
        let column_amplitude = |x: u32| {
            (0..40u32)
                .filter(|y| {
                    let o = ((y * 50 + x) * 4) as usize;
                    data[o] >= 250 && data[o + 3] == 255
                })
                .count()
        };
        // Outside the region: background only (no full-bright pixels).
        assert_eq!(column_amplitude(5), 0);
        assert_eq!(column_amplitude(35), 0);
        // Mid-region (avoid the border columns): tall full-scale waveform.
        assert!(column_amplitude(20) > 30, "burst drawn inside the region");
    }

    #[test]
    fn region_starting_mid_column_stays_grid_aligned() {
        // Burst only in source [832, 848) — SLOT-ALIGNED (level-0 folds 8
        // frames, so unaligned boundaries legitimately bleed one slot into a
        // neighbouring column). Region timeline_start 210 with fpp 20 starts
        // half way through column 10; the bulk grid after the partial head
        // column must stay exactly aligned: the burst maps into column 12
        // alone. The pre-fix half-column source offset smeared it into
        // column 11 (which then read source 816..840).
        let mut samples = vec![0.0f32; 2000];
        for (i, s) in samples[832..848].iter_mut().enumerate() {
            *s = if i % 2 == 0 { 1.0 } else { -1.0 };
        }
        let pyramid = WavePyramid::new(&samples);
        let region = WaveRegion {
            timeline_start: 210,
            source_start: 800,
            len: 400,
            gain: 1.0,
            fade_in: 0,
            fade_out: 0,
            selected: false,
        };
        let image = paint_region_lane(RegionLanePaintParams {
            pyramid: &pyramid,
            regions: &[region],
            start_frame: 0.0,
            frames_per_px: 20.0,
            width: 50,
            height: 40,
            color: Color::WHITE,
            background: Color::srgb_u8(23, 23, 28),
        });
        let data = image.data.as_ref().expect("cpu image data");
        let column_amplitude = |x: u32| {
            (0..40u32)
                .filter(|y| {
                    let o = ((y * 50 + x) * 4) as usize;
                    data[o] >= 250 && data[o + 3] == 255
                })
                .count()
        };
        // In-region silence draws the 1px centre hairline; the burst must
        // not smear beyond its own column.
        assert_eq!(column_amplitude(11), 1, "no smear into column 11");
        assert!(column_amplitude(12) > 30, "burst lands in column 12");
        assert_eq!(column_amplitude(13), 1, "no smear into column 13");
    }

    #[test]
    fn ruler_ticks_are_ordered_and_labelled() {
        let ticks = ruler_ticks(12.3, 0.05, 1200);
        assert!(!ticks.major.is_empty());
        let mut last = -1.0f32;
        for &(x, t) in &ticks.major {
            assert!(x > last && (0.0..1200.0).contains(&x));
            assert!((t / ticks.major_step_secs).fract().abs() < 1e-9);
            last = x;
        }
    }

    #[test]
    fn ruler_label_formats() {
        assert_eq!(format_ruler_secs(0.0, 5.0), "0:00");
        assert_eq!(format_ruler_secs(65.0, 5.0), "1:05");
        assert_eq!(format_ruler_secs(0.5, 0.5), "0:00.5");
    }
}
