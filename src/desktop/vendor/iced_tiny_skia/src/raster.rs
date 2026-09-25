use crate::core::image as raster;
use crate::core::{Rectangle, Size};
use crate::graphics;

use rustc_hash::{FxHashMap, FxHashSet};
use std::cell::RefCell;
use std::collections::hash_map;

#[cfg(feature = "raster-probe")]
std::thread_local! {
    static NATIVE_COPIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Return and reset this thread's successful image/grid native-placement copy count.
#[cfg(feature = "raster-probe")]
pub fn take_native_copy_count() -> usize {
    NATIVE_COPIES.with(|count| count.replace(0))
}

#[cfg(feature = "raster-probe")]
pub(crate) fn record_native_copy() {
    NATIVE_COPIES.with(|count| count.set(count.get() + 1));
}

#[derive(Debug)]
pub struct Pipeline {
    cache: RefCell<Cache>,
}

impl Pipeline {
    pub fn new() -> Self {
        Self {
            cache: RefCell::new(Cache::default()),
        }
    }

    pub fn load(
        &self,
        handle: &raster::Handle,
    ) -> Result<raster::Allocation, raster::Error> {
        let mut cache = self.cache.borrow_mut();
        let image = cache.allocate(handle)?;

        #[allow(unsafe_code)]
        Ok(unsafe {
            raster::allocate(handle, Size::new(image.width(), image.height()))
        })
    }

    pub fn dimensions(&self, handle: &raster::Handle) -> Option<Size<u32>> {
        let mut cache = self.cache.borrow_mut();
        let image = cache.allocate(handle).ok()?;

        Some(Size::new(image.width(), image.height()))
    }

    pub fn draw(
        &mut self,
        handle: &raster::Handle,
        filter_method: raster::FilterMethod,
        bounds: Rectangle,
        opacity: f32,
        pixels: &mut tiny_skia::PixmapMut<'_>,
        transform: tiny_skia::Transform,
        clip_mask: Option<&tiny_skia::Mask>,
        clip_bounds: Rectangle,
    ) {
        let mut cache = self.cache.borrow_mut();

        let Ok(image) = cache.allocate(handle) else {
            return;
        };

        let width_scale = bounds.width / image.width() as f32;
        let height_scale = bounds.height / image.height() as f32;
        let effective = transform.pre_scale(width_scale, height_scale);
        // Test the combined transform: image scaling can cancel output scale.
        let negative_identity = effective.is_identity() && (bounds.x < 0.0 || bounds.y < 0.0);

        // Resolve cumulative logical edges in physical space before upstream's
        // per-image scaling and truncation. Float cancellation at fractional
        // output scales must not turn a native-size band into a resampled one.
        if !cfg!(feature = "reference-raster") && image.opaque && opacity == 1.0 && !negative_identity
            && let Some(placed) = native_placement(bounds, transform, image.width(), image.height())
            && copy_opaque(image.pixmap(), pixels, placed, clip_bounds)
        {
            #[cfg(feature = "raster-probe")]
            record_native_copy();
            return;
        }

        let transform = effective;

        // tiny-skia's identity fill_rect rounds negative local bounds
        // differently from its transformed path; retain that edge behaviour.
        if !cfg!(feature = "reference-raster") && image.opaque && opacity == 1.0 && !negative_identity {
            let placed = transform.pre_translate(
                (bounds.x / width_scale) as i32 as f32,
                (bounds.y / height_scale) as i32 as f32,
            );
            if copy_opaque(image.pixmap(), pixels, placed, clip_bounds) {
                return;
            }
        }

        let quality = match filter_method {
            raster::FilterMethod::Linear => tiny_skia::FilterQuality::Bilinear,
            raster::FilterMethod::Nearest => tiny_skia::FilterQuality::Nearest,
        };

        pixels.draw_pixmap(
            (bounds.x / width_scale) as i32,
            (bounds.y / height_scale) as i32,
            image.pixmap(),
            &tiny_skia::PixmapPaint {
                quality,
                opacity,
                ..Default::default()
            },
            transform,
            clip_mask,
        );
    }

    pub fn trim_cache(&mut self) {
        self.cache.borrow_mut().trim();
    }
}

#[derive(Debug, Default)]
struct Cache {
    entries: FxHashMap<raster::Id, Option<Entry>>,
    hits: FxHashSet<raster::Id>,
}

impl Cache {
    pub fn allocate(
        &mut self,
        handle: &raster::Handle,
    ) -> Result<&Entry, raster::Error> {
        let id = handle.id();

        if let hash_map::Entry::Vacant(entry) = self.entries.entry(id) {
            let image = match graphics::image::load(handle) {
                Ok(image) => image,
                Err(error) => {
                    let _ = entry.insert(None);

                    return Err(error);
                }
            };

            if image.width() == 0 || image.height() == 0 {
                return Err(raster::Error::Empty);
            }

            let mut buffer =
                vec![0u32; image.width() as usize * image.height() as usize];

            let mut opaque = true;
            for (i, pixel) in image.pixels().enumerate() {
                let [r, g, b, a] = pixel.0;
                opaque &= a == 255;

                buffer[i] = bytemuck::cast(
                    tiny_skia::ColorU8::from_rgba(b, g, r, a).premultiply(),
                );
            }

            let _ = entry.insert(Some(Entry {
                width: image.width(),
                height: image.height(),
                pixels: buffer,
                opaque,
            }));
        }

        let _ = self.hits.insert(id);

        Ok(self
            .entries
            .get(&id)
            .unwrap()
            .as_ref()
            .expect("Image should be allocated"))
    }

    fn trim(&mut self) {
        self.entries.retain(|key, _| self.hits.contains(key));
        self.hits.clear();
    }
}

#[derive(Debug)]
struct Entry {
    width: u32,
    height: u32,
    pixels: Vec<u32>,
    opaque: bool,
}

impl Entry {
    fn width(&self) -> u32 {
        self.width
    }
    fn height(&self) -> u32 {
        self.height
    }
    fn pixmap(&self) -> tiny_skia::PixmapRef<'_> {
        tiny_skia::PixmapRef::from_bytes(
            bytemuck::cast_slice(&self.pixels),
            self.width,
            self.height,
        )
        .expect("Build pixmap from image bytes")
    }
}

// Only absorb float round-off, not genuine fractional placement or scaling.
pub(crate) fn native_placement(
    bounds: Rectangle,
    transform: tiny_skia::Transform,
    width: u32,
    height: u32,
) -> Option<tiny_skia::Transform> {
    if transform.kx != 0.0 || transform.ky != 0.0
        || transform.sx <= 0.0 || transform.sy <= 0.0
    {
        return None;
    }
    let edges = [
        bounds.x * transform.sx + transform.tx,
        bounds.y * transform.sy + transform.ty,
        (bounds.x + bounds.width) * transform.sx + transform.tx,
        (bounds.y + bounds.height) * transform.sy + transform.ty,
    ];
    if !edges.iter().all(|v| v.is_finite()
        && (v - v.round()).abs() <= (v.abs().max(1.0) * f32::EPSILON * 4.0).min(0.01)) {
        return None;
    }
    let [left, top, right, bottom] = edges.map(f32::round);
    if right - left != width as f32 || bottom - top != height as f32 {
        return None;
    }
    Some(tiny_skia::Transform::from_translate(left, top))
}

/// Native premultiplied pixels; only exact translations qualify. Rectangle
/// coverage matches the non-antialiased mask's 26.6 scan conversion.
/// Also used by native Source grids, where replacing alpha is intentional.
pub(crate) fn copy_opaque(
    image: tiny_skia::PixmapRef<'_>,
    target: &mut tiny_skia::PixmapMut<'_>,
    transform: tiny_skia::Transform,
    clip: Rectangle,
) -> bool {
    if transform.sx != 1.0
        || transform.sy != 1.0
        || transform.kx != 0.0
        || transform.ky != 0.0
        || ![transform.tx, transform.ty]
        .iter()
        .all(|v| v.is_finite() && v.fract() == 0.0)
        || ![clip.x, clip.y, clip.x + clip.width, clip.y + clip.height]
            .iter().all(|v| v.is_finite() && v.abs() < 32767.0)
    {
        return false;
    }
    // i64 keeps translated bounds safe even for saturated float-to-int casts.
    let x = transform.tx as i32 as i64;
    let y = transform.ty as i32 as i64;
    // LineEdge::new truncates to 26.6, then fdot6::round adds half a
    // pixel. Plain f32::round differs near negative half-pixel edges.
    let edge = |v: f32| i64::from(((v * 64.0) as i32 + 32) >> 6);
    let left = x.max(0).max(edge(clip.x));
    let top = y.max(0).max(edge(clip.y));
    let right = (x + i64::from(image.width()))
        .min(i64::from(target.width()))
        .min(edge(clip.x + clip.width));
    let bottom = (y + i64::from(image.height()))
        .min(i64::from(target.height()))
        .min(edge(clip.y + clip.height));
    if left >= right || top >= bottom {
        return true;
    }
    let source_stride = image.width() as usize * 4;
    let target_stride = target.width() as usize * 4;
    let length = (right - left) as usize * 4;
    for row in top..bottom {
        let source = (row - y) as usize * source_stride + (left - x) as usize * 4;
        let destination = row as usize * target_stride + left as usize * 4;
        target.data_mut()[destination..destination + length]
            .copy_from_slice(&image.data()[source..source + length]);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_unmasked_native_image_remains_under_later_clipped_overlay() {
        use crate::core::{Renderer as _, image::Renderer as _};
        let viewport = crate::graphics::Viewport::with_physical_size(Size::new(15, 15), 1.0);
        let clip = Rectangle::with_size(viewport.logical_size());
        let bounds = Rectangle { x: 2.0, y: 3.0, width: 7.0, height: 5.0 };
        let overlay = Rectangle { x: 4.0, y: 4.0, width: 3.0, height: 2.0 };
        let bytes: Vec<_> = (0..35).flat_map(|i|
            [(i * 53) as u8, (i * 97) as u8, (i * 13) as u8, 255]).collect();
        let handle = raster::Handle::from_rgba(7, 5, bytes.clone());
        let mut renderer = crate::Renderer::new(crate::core::Font::default(), crate::core::Pixels(13.0));
        renderer.reset(clip);
        renderer.draw_image(raster::Image::new(handle), bounds, clip);
        renderer.start_layer(overlay);
        renderer.fill_quad(crate::core::renderer::Quad { bounds: clip, ..Default::default() }, crate::core::Color::WHITE);
        renderer.end_layer();
        let mut pixels = tiny_skia::Pixmap::new(15, 15).unwrap();
        let mut mask = tiny_skia::Mask::new(15, 15).unwrap();
        renderer.draw(&mut pixels.as_mut(), &mut mask, &viewport, &[clip], crate::core::Color::BLACK);
        for y in 0..15usize {
            for x in 0..15usize {
                let expected = if (4..7).contains(&x) && (4..6).contains(&y) {
                    [255; 4]
                } else if (2..9).contains(&x) && (3..8).contains(&y) {
                    let i = ((y - 3) * 7 + x - 2) * 4;
                    [bytes[i + 2], bytes[i + 1], bytes[i], 255]
                } else { [0, 0, 0, 255] };
                assert_eq!(&pixels.data()[(y * 15 + x) * 4..(y * 15 + x + 1) * 4], &expected,
                    "pixel {x},{y}");
            }
        }
    }

    #[test]
    fn cancelled_scale_negative_origin_preserves_upstream_pixels() {
        let handle = raster::Handle::from_rgba(7, 5,
            (0..35).flat_map(|i| [(i * 53) as u8, (i * 97) as u8, (i * 13) as u8, 255]).collect::<Vec<_>>());
        for scale in [1.0, 2.0, 4.0] {
            for origin in [-0.5, -1.0, -3.0] {
                let bounds = Rectangle { x: origin, y: 0.0, width: 7.0 / scale, height: 5.0 / scale };
                let clip = Rectangle::with_size(Size::new(15.0, 15.0));
                let mut actual = tiny_skia::Pixmap::new(15, 15).unwrap();
                let mut expected = actual.clone();
                let mut fast = Pipeline::new();
                let mut original = Pipeline::new();
                let _ = original.load(&handle).unwrap();
                original.cache.borrow_mut().entries.get_mut(&handle.id()).unwrap().as_mut().unwrap().opaque = false;
                for (pipeline, target) in [(&mut fast, &mut actual), (&mut original, &mut expected)] {
                    pipeline.draw(&handle, raster::FilterMethod::Nearest, bounds, 1.0,
                        &mut target.as_mut(), tiny_skia::Transform::from_scale(scale, scale), None, clip);
                }
                assert_eq!(actual.data(), expected.data(), "scale={scale} origin={origin}");
            }
        }
    }

    #[test]
    fn large_coordinates_tolerate_roundoff_but_not_fractional_placement() {
        let bounds = Rectangle { x: 4096.0005, y: 8192.001, width: 7.0, height: 5.0 };
        assert!(native_placement(bounds, tiny_skia::Transform::identity(), 7, 5).is_some());
        assert!(native_placement(Rectangle { x: 4096.25, ..bounds }, tiny_skia::Transform::identity(), 7, 5).is_none());
        assert!(native_placement(Rectangle { x: 0.0, y: 0.0, width: 7.0, height: 5.0 },
            tiny_skia::Transform::from_scale(2.0, 2.0), 7, 5).is_none());
    }

    #[test]
    fn cumulative_band_edges_use_native_copy_at_seven_scales() {
        for scale in [1.0, 1.1, 1.25, 1.5, 1.75, 2.25, 2.5] {
            for offset in [0.0, 1.0, 3.0, 17.0, 30.0] {
                for first in (0..61).step_by(4) {
                    let top = first * 41;
                    let bottom = (first + 4).min(61) * 41;
                    let bounds = Rectangle {
                        x: offset / scale,
                        y: top as f32 / scale + offset / scale,
                        width: 180.0 / scale,
                        height: bottom as f32 / scale - top as f32 / scale,
                    };
                    assert_eq!(
                        native_placement(bounds, tiny_skia::Transform::from_scale(scale, scale), 180, bottom - top),
                        Some(tiny_skia::Transform::from_translate(offset, top as f32 + offset)),
                        "scale={scale} offset={offset} row={first}",
                    );
                }
            }
        }
    }

    #[test]
    fn pipeline_matches_forced_fallback_for_placement_and_fractional_clips() {
        let handle = raster::Handle::from_rgba(7, 5,
            (0..35).flat_map(|i| [(i * 53) as u8, (i * 97) as u8, (i * 13) as u8, 255]).collect::<Vec<_>>());
        for origin in [-3.0, 0.0, 4.0] {
            for edge in [-2.51, -2.5, -2.49, 0.0, 0.49, 0.5, 0.51, 1.25, 2.5] {
                for transform in [
                    tiny_skia::Transform::identity(),
                    tiny_skia::Transform::from_translate(2.0, 3.0),
                    tiny_skia::Transform::from_rotate(15.0),
                ] {
                    let bounds = Rectangle { x: origin, y: origin, width: 7.0, height: 5.0 };
                    let clip = Rectangle { x: edge, y: edge, width: 8.0, height: 8.0 };
                    let mut mask = tiny_skia::Mask::new(15, 15).unwrap();
                    crate::engine::adjust_clip_mask(&mut mask, clip);
                    let mut actual = tiny_skia::Pixmap::new(15, 15).unwrap();
                    let mut expected = actual.clone();
                    let mut fast = Pipeline::new();
                    let mut original = Pipeline::new();
                    let _ = original.load(&handle).unwrap();
                    original.cache.borrow_mut().entries.get_mut(&handle.id())
                        .unwrap().as_mut().unwrap().opaque = false;
                    for (pipeline, target) in [(&mut fast, &mut actual), (&mut original, &mut expected)] {
                        pipeline.draw(&handle, raster::FilterMethod::Linear, bounds, 1.0,
                            &mut target.as_mut(), transform, Some(&mask), clip);
                    }
                    assert_eq!(actual.data(), expected.data(), "origin={origin}, clip={edge}, transform={transform:?}");
                }
            }
        }
    }

    #[test]
    fn native_copy_matches_pattern_with_offsets_clips_and_colour_extremes() {
        let mut source = tiny_skia::Pixmap::new(7, 5).unwrap();
        for (i, pixel) in source.pixels_mut().iter_mut().enumerate() {
            *pixel =
                tiny_skia::ColorU8::from_rgba((i * 53) as u8, (i * 97) as u8, (i * 13) as u8, 255)
                    .premultiply();
        }
        for (x, y) in [(0, 0), (3, 4), (-3, -2), (10, 8)] {
            for clip in [
                Rectangle {
                    x: 0.0,
                    y: 0.0,
                    width: 13.0,
                    height: 11.0,
                },
                Rectangle {
                    x: 2.0,
                    y: 3.0,
                    width: 5.0,
                    height: 4.0,
                },
            ] {
                let mut original = tiny_skia::Pixmap::new(13, 11).unwrap();
                original.fill(tiny_skia::Color::from_rgba8(23, 45, 67, 255));
                let mut copied = original.clone();
                let mut mask = tiny_skia::Mask::new(13, 11).unwrap();
                crate::engine::adjust_clip_mask(&mut mask, clip);
                original.draw_pixmap(
                    0,
                    0,
                    source.as_ref(),
                    &tiny_skia::PixmapPaint::default(),
                    tiny_skia::Transform::from_translate(x as f32, y as f32),
                    Some(&mask),
                );
                assert!(copy_opaque(
                    source.as_ref(),
                    &mut copied.as_mut(),
                    tiny_skia::Transform::from_translate(x as f32, y as f32),
                    clip
                ));
                assert_eq!(
                    copied.data(),
                    original.data(),
                    "offset {x},{y}, clip {clip:?}"
                );
            }
        }
    }

    #[test]
    fn scaled_translucent_and_fractional_draws_keep_original_path() {
        for alpha in [128, 255] {
            let handle = raster::Handle::from_rgba(7, 5, [31, 113, 241, alpha].repeat(35));
            for scale in [1.0, 1.25, 1.5, 2.0, 2.5] {
                for offset in [0.0, 0.25] {
                    for opacity in [0.5, 1.0] {
                        let bounds = Rectangle {
                            x: 2.0,
                            y: 3.0,
                            width: 7.0,
                            height: 5.0,
                        };
                        let clip = Rectangle {
                            x: 1.0,
                            y: 1.0,
                            width: 18.0,
                            height: 17.0,
                        };
                        let transform = tiny_skia::Transform::from_scale(scale, scale)
                            .post_translate(offset, offset);
                        let mut expected = tiny_skia::Pixmap::new(20, 20).unwrap();
                        expected.fill(tiny_skia::Color::from_rgba8(4, 5, 6, 255));
                        let mut actual = expected.clone();
                        let mut mask = tiny_skia::Mask::new(20, 20).unwrap();
                        crate::engine::adjust_clip_mask(&mut mask, clip);
                        let mut pipeline = Pipeline::new();
                        {
                            let mut cache = pipeline.cache.borrow_mut();
                            let image = cache.allocate(&handle).unwrap();
                            assert_eq!(image.opaque, alpha == 255);
                            expected.draw_pixmap(
                                2,
                                3,
                                image.pixmap(),
                                &tiny_skia::PixmapPaint {
                                    opacity,
                                    quality: tiny_skia::FilterQuality::Nearest,
                                    ..Default::default()
                                },
                                transform,
                                Some(&mask),
                            );
                            if scale != 1.0 || offset != 0.0 {
                                assert!(!copy_opaque(
                                    image.pixmap(),
                                    &mut actual.as_mut(),
                                    transform,
                                    clip
                                ));
                            }
                        }
                        pipeline.draw(
                            &handle,
                            raster::FilterMethod::Nearest,
                            bounds,
                            opacity,
                            &mut actual.as_mut(),
                            transform,
                            Some(&mask),
                            clip,
                        );
                        assert_eq!(
                            actual.data(),
                            expected.data(),
                            "scale={scale} alpha={alpha} opacity={opacity} offset={offset}"
                        );
                    }
                }
            }
        }
    }
}
