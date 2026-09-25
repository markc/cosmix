//! Immutable native grid generations, outside the generic image cache.
use crate::core::{Bytes, Rectangle};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

fn next_generation() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
        n.checked_add(1)
    })
    .expect("native grid generation exhausted")
}

/// Bounded cell revision metadata, independent of pixel storage. Cloning this
/// never retains a pixel buffer. Producers MUST mark every written rectangle
/// before publishing new pixels with `Grid::with_damage`.
#[derive(Clone)]
pub struct Damage {
    lineage: u64,
    width: u32,
    height: u32,
    cell: (u32, u32),
    revisions: Arc<[u64]>,
}

impl Damage {
    pub fn new(width: u32, height: u32, cell: (u32, u32)) -> Option<Self> {
        if width == 0 || height == 0 || cell.0 == 0 || cell.1 == 0 {
            return None;
        }
        let count = (width.div_ceil(cell.0) as usize)
            .checked_mul(height.div_ceil(cell.1) as usize)?;
        let lineage = next_generation();
        Some(Self {
            lineage,
            width,
            height,
            cell,
            revisions: vec![lineage; count].into(),
        })
    }

    pub fn mark<R: std::borrow::Borrow<Rectangle<u32>>, I>(
        &mut self,
        rectangles: I,
    ) where
        I: IntoIterator<Item = R>,
        I::IntoIter: Clone,
    {
        let revision = next_generation();
        let cols = self.width.div_ceil(self.cell.0) as usize;
        let rectangles = rectangles.into_iter();
        if rectangles.clone().any(|rect| {
            let rect = rect.borrow();
            rect.x == 0
                && rect.y == 0
                && rect.width >= self.width
                && rect.height >= self.height
        }) {
            // Retained generations need their old stamps, but a full
            // overwrite has no reason to copy those stamps first.
            self.revisions = vec![revision; self.revisions.len()].into();
            return;
        }
        let revisions = Arc::make_mut(&mut self.revisions);
        for rect in rectangles {
            let rect = rect.borrow();
            if rect.width == 0 || rect.height == 0 {
                continue;
            }
            let right = rect
                .x
                .saturating_add(rect.width)
                .min(self.width)
                .div_ceil(self.cell.0);
            let bottom = rect
                .y
                .saturating_add(rect.height)
                .min(self.height)
                .div_ceil(self.cell.1);
            for row in rect.y / self.cell.1..bottom {
                for col in rect.x / self.cell.0..right {
                    revisions[row as usize * cols + col as usize] = revision;
                }
            }
        }
    }
}

/// A tightly packed premultiplied BGRA8 grid (stride = width * 4).
///
/// This primitive replaces destination pixels (Source blending). Producers
/// must supply premultiplied channels; opaque terminal pixels satisfy this
/// without arithmetic. Unlike an image, this is not a SourceOver operation.
/// Clones retain immutable generation-owned bytes, including in age history.
#[derive(Clone)]
pub struct Grid {
    generation: u64,
    pixels: Bytes,
    width: u32,
    height: u32,
    damage: Damage,
}

impl std::fmt::Debug for Grid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Grid")
            .field("generation", &self.generation)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("byte_len", &self.pixels.len())
            .finish()
    }
}

impl Grid {
    pub fn new(width: u32, height: u32, pixels: Bytes) -> Option<Self> {
        Self::with_damage(pixels, &Damage::new(width, height, (width, height))?)
    }

    /// Publish immutable pixels and a snapshot of their cell revisions.
    pub fn with_damage(pixels: Bytes, damage: &Damage) -> Option<Self> {
        let (width, height) = (damage.width, damage.height);
        let len = (width as usize)
            .checked_mul(height as usize)?
            .checked_mul(4)?;
        if width == 0 || height == 0 || pixels.len() != len {
            return None;
        }
        let _ = tiny_skia::PixmapRef::from_bytes(&pixels, width, height)?;
        let generation = next_generation();
        Some(Self {
            generation,
            pixels,
            width,
            height,
            damage: damage.clone(),
        })
    }

    /// Compare against ANY retained age, without an ancestry chain or a pixel
    /// scan. Unrelated grids require full damage. Reverting a cell still
    /// changes its stamp, conservatively covering A -> B -> A presentation.
    pub(crate) fn damage_since(
        &self,
        old: &Self,
        bounds: Rectangle,
    ) -> Vec<Rectangle> {
        if self == old {
            return Vec::new();
        }
        let new = &self.damage;
        let old = &old.damage;
        if new.lineage != old.lineage
            || new.cell != old.cell
            || new.width != old.width
            || new.height != old.height
        {
            return vec![bounds.expand(1.0)];
        }
        let cols = new.width.div_ceil(new.cell.0) as usize;
        let mut regions: Vec<Rectangle<u32>> = Vec::new();
        let limit = 2 * new.height.div_ceil(new.cell.1) as usize;
        let mut previous: Vec<usize> = Vec::new();
        let mut current = Vec::new();
        'rows: for (row, (now, before)) in new
            .revisions
            .chunks(cols)
            .zip(old.revisions.chunks(cols))
            .enumerate()
        {
            current.clear();
            let mut candidate = 0;
            let mut col = 0;
            while col < cols {
                if now[col] == before[col] {
                    col += 1;
                    continue;
                }
                let first = col;
                while col < cols && now[col] != before[col] {
                    col += 1;
                }
                let x = first as u32 * new.cell.0;
                let y = row as u32 * new.cell.1;
                let width = (col as u32 * new.cell.0).min(new.width) - x;
                let height = new.cell.1.min(new.height - y);
                while candidate < previous.len()
                    && regions[previous[candidate]].x < x
                {
                    candidate += 1;
                }
                if let Some(&index) = previous.get(candidate)
                    && regions[index].x == x
                    && regions[index].width == width
                {
                    regions[index].height += height;
                    current.push(index);
                } else {
                    current.push(regions.len());
                    regions.push(Rectangle {
                        x,
                        y,
                        width,
                        height,
                    });
                    if regions.len() > limit {
                        regions.clear();
                        for (row, (now, before)) in new
                            .revisions
                            .chunks(cols)
                            .zip(old.revisions.chunks(cols))
                            .enumerate()
                        {
                            if now == before {
                                continue;
                            }
                            let y = row as u32 * new.cell.1;
                            let height = new.cell.1.min(new.height - y);
                            if let Some(last) = regions.last_mut()
                                && last.y + last.height == y
                            {
                                last.height += height;
                            } else {
                                regions.push(Rectangle {
                                    x: 0,
                                    y,
                                    width: new.width,
                                    height,
                                });
                            }
                        }
                        break 'rows;
                    }
                }
            }
            std::mem::swap(&mut previous, &mut current);
        }
        let sx = bounds.width / new.width as f32;
        let sy = bounds.height / new.height as f32;
        // The generic nearest-neighbour fallback truncates its source origin.
        // Cover up to one scaled source pixel as well as iced's usual margin.
        let margin = 1.0_f32.max(sx.abs()).max(sy.abs());
        regions
            .into_iter()
            .map(|r| {
                Rectangle {
                    x: bounds.x + r.x as f32 * sx,
                    y: bounds.y + r.y as f32 * sy,
                    width: r.width as f32 * sx,
                    height: r.height as f32 * sy,
                }
                .expand(margin)
            })
            .collect()
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
        // Mirror the image path (raster.rs Pipeline::draw): tiny-skia's
        // identity fill rounds negative local bounds differently from its
        // transformed path, so a negative origin under a combined identity
        // transform must take the fallback, or grids and images diverge.
        let effective = transform.pre_scale(
            bounds.width / self.width as f32,
            bounds.height / self.height as f32,
        );
        if effective.is_identity() && (bounds.x < 0.0 || bounds.y < 0.0) {
            return false;
        }
        let copied = crate::raster::native_placement(
            bounds,
            transform,
            self.width,
            self.height,
        )
        .is_some_and(|placed| {
            crate::raster::copy_opaque(pixmap, target, placed, clip)
        });
        #[cfg(feature = "raster-probe")]
        if copied {
            crate::raster::record_native_copy();
        }
        copied
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
    fn full_damage_replaces_shared_stamps_without_changing_retained_history() {
        let mut damage = Damage::new(8, 6, (2, 2)).unwrap();
        let old = damage.clone();
        damage.mark([Rectangle {
            x: 0,
            y: 0,
            width: 8,
            height: 6,
        }]);
        assert!(!Arc::ptr_eq(&old.revisions, &damage.revisions));
        assert!(old.revisions.iter().all(|&r| r == old.lineage));
        assert!(
            damage
                .revisions
                .iter()
                .all(|&r| r == damage.revisions[0] && r != old.lineage)
        );
    }

    #[test]
    fn revision_damage_bounds_checkerboards_and_preserves_open_runs() {
        let (cols, rows) = (96, 32);
        let mut damage = Damage::new(cols, rows, (1, 1)).unwrap();
        let pixels = Bytes::from([0, 0, 0, 255].repeat((cols * rows) as usize));
        let old = Grid::with_damage(pixels.clone(), &damage).unwrap();
        let bounds = Rectangle {
            x: 0.0,
            y: 0.0,
            width: cols as f32,
            height: rows as f32,
        };
        damage.mark([
            Rectangle {
                x: 0,
                y: 0,
                width: 1,
                height: 3,
            },
            Rectangle {
                x: 2,
                y: 1,
                width: 1,
                height: 2,
            },
        ]);
        let next = Grid::with_damage(pixels.clone(), &damage).unwrap();
        assert_eq!(
            next.damage_since(&old, bounds),
            vec![
                Rectangle {
                    x: -1.0,
                    y: -1.0,
                    width: 3.0,
                    height: 5.0
                },
                Rectangle {
                    x: 1.0,
                    y: 0.0,
                    width: 3.0,
                    height: 4.0
                },
            ]
        );
        let mut rectangles = Vec::new();
        for y in 0..rows {
            for x in 0..cols {
                if y != 15 && (x + y) % 2 == 0 {
                    rectangles.push(Rectangle {
                        x,
                        y,
                        width: 1,
                        height: 1,
                    });
                }
            }
        }
        damage.mark(rectangles.iter());
        let next = Grid::with_damage(pixels, &damage).unwrap();
        let regions = next.damage_since(&old, bounds);
        assert!(regions.len() <= 2 * rows as usize);
        assert_eq!(
            regions,
            vec![
                Rectangle {
                    x: -1.0,
                    y: -1.0,
                    width: 98.0,
                    height: 17.0
                },
                Rectangle {
                    x: -1.0,
                    y: 15.0,
                    width: 98.0,
                    height: 18.0
                },
            ]
        );
    }

    #[test]
    fn cell_revisions_cover_arbitrary_ages_reverts_and_skipped_publications() {
        let bounds = Rectangle {
            x: 7.0,
            y: 9.0,
            width: 90.0,
            height: 20.0,
        };
        let mut damage = Damage::new(90, 20, (3, 5)).unwrap();
        let mut bytes = vec![0; 90 * 20 * 4];
        let first =
            Grid::with_damage(Bytes::from(bytes.clone()), &damage).unwrap();
        let a = Rectangle {
            x: 6,
            y: 5,
            width: 3,
            height: 5,
        };
        let b = Rectangle {
            x: 75,
            y: 15,
            width: 3,
            height: 5,
        };
        bytes[(5 * 90 + 6) * 4] = 255;
        damage.mark([a]);
        let second =
            Grid::with_damage(Bytes::from(bytes.clone()), &damage).unwrap();
        assert_eq!(
            second.damage_since(&first, bounds),
            vec![Rectangle {
                x: 12.0,
                y: 13.0,
                width: 5.0,
                height: 7.0,
            }]
        );
        bytes[(5 * 90 + 6) * 4] = 0; // A -> B -> A still owes display damage.
        damage.mark([a]);
        damage.mark([b]); // two updates before publishing, no lost range
        let third = Grid::with_damage(Bytes::from(bytes), &damage).unwrap();
        assert_eq!(third.damage_since(&first, bounds).len(), 2);
        assert_eq!(third.damage_since(&second, bounds).len(), 2);
        assert!(third.damage_since(&third.clone(), bounds).is_empty());
        assert_eq!(first.pixels()[(5 * 90 + 6) * 4], 0);
        assert_eq!(second.pixels()[(5 * 90 + 6) * 4], 255);
        let unrelated = Grid::new(90, 20, first.pixels().clone()).unwrap();
        assert_eq!(
            third.damage_since(&unrelated, bounds),
            vec![bounds.expand(1.0)]
        );
    }

    #[test]
    fn layer_diff_preserves_narrow_cell_damage() {
        use crate::core::Renderer as _;
        let clip = Rectangle {
            x: 0.0,
            y: 0.0,
            width: 300.0,
            height: 200.0,
        };
        let bounds = Rectangle {
            x: 10.0,
            y: 10.0,
            width: 180.0,
            height: 40.0,
        };
        let mut damage = Damage::new(180, 40, (2, 10)).unwrap();
        let pixels = Bytes::from([0, 0, 0, 255].repeat(180 * 40));
        let mut renderer = crate::Renderer::new(
            crate::core::Font::default(),
            crate::core::Pixels(13.0),
        );
        renderer.reset(clip);
        renderer.draw_grid(
            Grid::with_damage(pixels.clone(), &damage).unwrap(),
            bounds,
            clip,
        );
        let previous = renderer.layers()[0].clone();
        damage.mark([Rectangle {
            x: 50,
            y: 10,
            width: 2,
            height: 10,
        }]);
        renderer.reset(clip);
        renderer.draw_grid(
            Grid::with_damage(pixels, &damage).unwrap(),
            bounds,
            clip,
        );
        assert_eq!(
            crate::Layer::damage(&previous, &renderer.layers()[0]),
            vec![Rectangle {
                x: 59.0,
                y: 19.0,
                width: 4.0,
                height: 12.0,
            }]
        );
    }

    #[test]
    fn layer_draw_intersects_widget_clip_and_restores_image_mask() {
        use crate::core::Renderer as _;
        use crate::core::image::Renderer as _;

        let layer_clip = Rectangle {
            x: 4.0,
            y: 4.0,
            width: 24.0,
            height: 24.0,
        };
        let widget_clip = Rectangle {
            x: 8.0,
            y: 8.0,
            width: 4.0,
            height: 4.0,
        };
        let grid_bounds = Rectangle {
            x: 0.0,
            y: 0.0,
            width: 16.0,
            height: 16.0,
        };
        let image_bounds = Rectangle {
            x: 20.0,
            y: 0.0,
            width: 16.0,
            height: 32.0,
        };
        let grid =
            Grid::new(16, 16, Bytes::from([0, 255, 0, 255].repeat(16 * 16)))
                .unwrap();
        // Translucency prevents the ordinary image's opaque-copy shortcut;
        // crossing the layer boundary forces it to consume the shared mask.
        let handle = crate::core::image::Handle::from_rgba(
            16,
            32,
            [255, 0, 0, 128].repeat(16 * 32),
        );
        for scale in [1.0, 1.25] {
            let viewport = crate::graphics::Viewport::with_physical_size(
                crate::core::Size::new(48, 48),
                scale,
            );
            let full = Rectangle::with_size(viewport.logical_size());
            let mut renderer = crate::Renderer::new(
                crate::core::Font::default(),
                crate::core::Pixels(13.0),
            );
            renderer.reset(full);
            renderer.with_layer(layer_clip, |renderer| {
                renderer.draw_grid(grid.clone(), grid_bounds, widget_clip);
                let mut image = crate::core::Image::new(handle.clone());
                image.filter_method = crate::core::image::FilterMethod::Nearest;
                renderer.draw_image(image, image_bounds, layer_clip);
            });
            assert_eq!(
                crate::raster::native_placement(
                    grid_bounds,
                    tiny_skia::Transform::from_scale(scale, scale),
                    16,
                    16,
                )
                .is_some(),
                scale == 1.0,
                "exercise native copy at 1.0 and masked fallback at 1.25"
            );
            let mut actual = tiny_skia::Pixmap::new(48, 48).unwrap();
            let mut mask = tiny_skia::Mask::new(48, 48).unwrap();
            renderer.draw(
                &mut actual.as_mut(),
                &mut mask,
                &viewport,
                &[full],
                crate::core::Color::TRANSPARENT,
            );

            // Independent per-pixel BGRA oracle: all edges are integral at
            // both scales, and the two solid-colour regions do not overlap.
            // The grid paints only [8,12) squared. The image paints only
            // [20,28) x [4,28), even though it is outside the widget clip.
            for y in 0..48 {
                for x in 0..48 {
                    let lx = (x as f32 + 0.5) / scale;
                    let ly = (y as f32 + 0.5) / scale;
                    let expected = if (8.0..12.0).contains(&lx)
                        && (8.0..12.0).contains(&ly)
                    {
                        [0, 255, 0, 255]
                    } else if (20.0..28.0).contains(&lx)
                        && (4.0..28.0).contains(&ly)
                    {
                        [0, 0, 128, 128]
                    } else {
                        [0, 0, 0, 0]
                    };
                    let offset = (y * 48 + x) * 4;
                    assert_eq!(
                        &actual.data()[offset..offset + 4],
                        &expected,
                        "scale={scale} pixel=({x},{y})"
                    );
                }
            }
        }
    }

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
            assert!(
                !crate::Layer::damage(&previous, &renderer.layers()[0])
                    .is_empty()
            );
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
