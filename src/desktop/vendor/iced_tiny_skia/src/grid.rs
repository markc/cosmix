//! Immutable native grid generations, outside the generic image cache.
use crate::core::{Bytes, Rectangle};
use std::sync::atomic::{AtomicU64, Ordering};

/// A tightly packed premultiplied BGRA8 grid (stride = width * 4).
///
/// This primitive replaces destination pixels (Source blending). Producers
/// must supply premultiplied channels; opaque terminal pixels satisfy this
/// without arithmetic. Unlike an image, this is not a SourceOver operation.
/// Clones retain immutable generation-owned bytes, including in age history.
#[derive(Clone, Debug)]
pub struct Grid {
    generation: u64,
    pixels: Bytes,
    width: u32,
    height: u32,
}

impl Grid {
    pub fn new(width: u32, height: u32, pixels: Bytes) -> Option<Self> {
        let len = (width as usize)
            .checked_mul(height as usize)?
            .checked_mul(4)?;
        if width == 0 || height == 0 || pixels.len() != len {
            return None;
        }
        let _ = tiny_skia::PixmapRef::from_bytes(&pixels, width, height)?;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let generation = NEXT
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                n.checked_add(1)
            })
            .expect("native grid generation exhausted");
        Some(Self {
            generation,
            pixels,
            width,
            height,
        })
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn pixels(&self) -> &Bytes {
        &self.pixels
    }
    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
    pub fn into_pixels(self) -> Bytes {
        self.pixels
    }

    pub(crate) fn copy(
        &self,
        bounds: Rectangle,
        target: &mut tiny_skia::PixmapMut<'_>,
        transform: tiny_skia::Transform,
        clip: Rectangle,
    ) -> bool {
        let pixmap = tiny_skia::PixmapRef::from_bytes(
            &self.pixels,
            self.width,
            self.height,
        )
        .expect("validated native grid dimensions");
        if let Some(placed) = crate::raster::native_placement(
            bounds,
            transform,
            self.width,
            self.height,
        ) {
            if crate::raster::copy_opaque(pixmap, target, placed, clip) {
                return true;
            }
        }
        false
    }

    pub(crate) fn draw_fallback(
        &self,
        bounds: Rectangle,
        target: &mut tiny_skia::PixmapMut<'_>,
        transform: tiny_skia::Transform,
        mask: Option<&tiny_skia::Mask>,
    ) {
        let pixmap = tiny_skia::PixmapRef::from_bytes(
            &self.pixels,
            self.width,
            self.height,
        )
        .expect("validated native grid dimensions");
        // Preserve the image path's placement/resampling outside the 1:1 case,
        // but do not load or convert an RGBA image even on this fallback.
        let sx = bounds.width / self.width as f32;
        let sy = bounds.height / self.height as f32;
        target.draw_pixmap(
            (bounds.x / sx) as i32,
            (bounds.y / sy) as i32,
            pixmap,
            &tiny_skia::PixmapPaint {
                quality: tiny_skia::FilterQuality::Nearest,
                blend_mode: tiny_skia::BlendMode::Source,
                ..Default::default()
            },
            transform.pre_scale(sx, sy),
            mask,
        );
    }
}

impl PartialEq for Grid {
    fn eq(&self, other: &Self) -> bool {
        self.generation == other.generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_damage_tracks_native_generation_placement_and_clip() {
        use crate::core::Renderer as _;
        let bounds = Rectangle {
            x: 3.0,
            y: 5.0,
            width: 7.0,
            height: 5.0,
        };
        let clip = Rectangle {
            x: 0.0,
            y: 0.0,
            width: 32.0,
            height: 32.0,
        };
        let pixels = Bytes::from([31, 47, 239, 255].repeat(35));
        let grid = Grid::new(7, 5, pixels.clone()).unwrap();
        let mut renderer = crate::Renderer::new(
            crate::core::Font::default(),
            crate::core::Pixels(13.0),
        );
        renderer.reset(clip);
        renderer.draw_grid(grid.clone(), bounds, clip);
        let previous = renderer.layers()[0].clone();
        renderer.reset(clip);
        renderer.draw_grid(grid.clone(), bounds, clip);
        assert!(
            crate::Layer::damage(&previous, &renderer.layers()[0]).is_empty()
        );
        for (next, placed, clipped) in [
            (Grid::new(7, 5, pixels).unwrap(), bounds, clip),
            (grid.clone(), Rectangle { x: 4.0, ..bounds }, clip),
            (grid, bounds, Rectangle { width: 8.0, ..clip }),
        ] {
            renderer.reset(clip);
            renderer.draw_grid(next, placed, clipped);
            assert!(!crate::Layer::damage(&previous, &renderer.layers()[0])
                .is_empty());
        }
    }

    #[test]
    fn generations_validate_shape_and_compare_without_scanning_pixels() {
        assert!(Grid::new(0, 1, Bytes::new()).is_none());
        assert!(Grid::new(2, 1, Bytes::from(vec![0; 7])).is_none());
        let pixels = Bytes::from(vec![31, 127, 239, 255]);
        let first = Grid::new(1, 1, pixels.clone()).unwrap();
        assert_eq!(first, first.clone());
        assert_ne!(first, Grid::new(1, 1, pixels.clone()).unwrap());
        assert_eq!(first.pixels().as_ptr(), pixels.as_ptr());
        assert!(
            first.into_pixels().try_into_mut().is_err(),
            "external owner still retains pixels"
        );
    }

    #[test]
    fn native_copy_and_fallback_match_rgba_pipeline() {
        let rgba: Vec<u8> = (0..35)
            .flat_map(|i| [i * 7, 255 - i * 3, i * 5, 255])
            .collect();
        let bgra: Vec<u8> = rgba
            .chunks_exact(4)
            .flat_map(|p| [p[2], p[1], p[0], p[3]])
            .collect();
        let grid = Grid::new(7, 5, Bytes::from(bgra)).unwrap();
        let handle = crate::core::image::Handle::from_rgba(7, 5, rgba);
        for scale in [1.0, 1.25, 2.5] {
            for x in [-3.0, 0.0, 4.25] {
                for edge in [0.0, 0.49, 0.5, 0.51, 1.25] {
                    let bounds = Rectangle {
                        x,
                        y: 2.0,
                        width: 7.0,
                        height: 5.0,
                    };
                    let transform =
                        tiny_skia::Transform::from_scale(scale, scale);
                    let clip = Rectangle {
                        x: edge,
                        y: edge,
                        width: 18.0,
                        height: 18.0,
                    };
                    let mut mask = tiny_skia::Mask::new(32, 32).unwrap();
                    crate::engine::adjust_clip_mask(&mut mask, clip);
                    let mut old = tiny_skia::Pixmap::new(32, 32).unwrap();
                    let mut new = old.clone();
                    crate::raster::Pipeline::new().draw(
                        &handle,
                        crate::core::image::FilterMethod::Nearest,
                        bounds,
                        1.0,
                        &mut old.as_mut(),
                        transform,
                        Some(&mask),
                        clip,
                    );
                    if !grid.copy(bounds, &mut new.as_mut(), transform, clip) {
                        grid.draw_fallback(
                            bounds,
                            &mut new.as_mut(),
                            transform,
                            Some(&mask),
                        );
                    }
                    assert_eq!(
                        old.data(),
                        new.data(),
                        "scale={scale} x={x} clip={edge}"
                    );
                }
            }
        }
    }
}
