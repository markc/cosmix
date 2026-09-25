//! Offscreen equivalent of compositor::present, including layer history and
//! damage grouping. Unlike Headless::screenshot this retains the target and
//! does not add a full-window screenshot allocation and BGRA→RGBA readback.
use super::*;
#[path = "cpu_rgba_reference.rs"]
mod rgba_reference;
use cosmix_term_core::{config::Cursor, terminal::Cell};
use iced::advanced::{Renderer as _, image::Renderer as _};
use iced::widget::image::{self, Handle};
use iced::{Color, Font, Pixels, Rectangle, Size};
use iced_tiny_skia::{
    Layer, Renderer,
    graphics::{Viewport, damage},
};
use rgba_reference::RgbaBand;
use std::time::Instant;

#[test]
fn band_widget_matches_exact_pixels_at_fractional_scales_and_offsets() {
    for (scale, cell_height) in [
        (1.0, 20),
        (1.1, 41),
        (1.25, 20),
        (1.25, 41),
        (1.5, 20),
        (1.5, 41),
        (1.75, 41),
        (2.0, 41),
        (2.25, 20),
        (2.25, 41),
        (2.5, 41),
    ] {
        let mut raster = Raster::new(scale, 13.0, Cursor::Underline).unwrap();
        raster.height = cell_height;
        let screen = Screen {
            cols: 9,
            rows: 61,
            cursor: (2, 4),
            cursor_visible: true,
            cells: (0..9 * 61)
                .map(|i| Cell {
                    c: 'M',
                    fg: [210, 220, 230],
                    bg: [20 + (i / 9) as u8, 25, 30],
                    bold: false,
                })
                .collect(),
            updated: Instant::now(),
        };
        let mut baseline = RgbaBand::default();
        baseline.paint(&mut raster, &screen, &[]);
        baseline.cache_handle(1);
        let mut surface = Surface::default();
        surface.paint(&mut raster, &screen, &[]);
        surface.cache_handle(1);
        let height = baseline.height + 64;
        let viewport = Viewport::with_physical_size(Size::new(800, height), scale);
        let clip = Rectangle::with_size(viewport.logical_size());
        let images = surface.images(scale);
        for pair in images.windows(2) {
            assert_eq!(pair[0].1.y + pair[0].1.height, pair[1].1.y);
        }
        let last = images.last().unwrap().1;
        assert_eq!(last.y + last.height, baseline.height as f32 / scale);
        for offset in [0.0, 1.0, 3.0, 17.0, 30.0] {
            let origin = iced::Point::new(offset / scale, offset / scale);
            let mut renderer = Renderer::new(Font::default(), Pixels(13.0));
            let mut mask = tiny_skia::Mask::new(800, height).unwrap();
            renderer.reset(clip);
            widget::draw_images(&mut renderer, &surface.images(scale), origin, scale, clip);
            let mut bands = tiny_skia::Pixmap::new(800, height).unwrap();
            #[cfg(feature = "raster-probe")]
            let _ = iced_tiny_skia::take_native_copy_count();
            renderer.draw(
                &mut bands.as_mut(),
                &mut mask,
                &viewport,
                &[clip],
                Color::BLACK,
            );
            // One full-pane region draws every native grid band exactly once.
            // Count successful native placement/copy, never draw_fallback;
            // pixel equality alone cannot prove routing.
            #[cfg(feature = "raster-probe")]
            assert_eq!(
                iced_tiny_skia::take_native_copy_count(),
                images.len(),
                "native copies at scale={scale} cell_height={cell_height} offset={offset}"
            );
            // Compare to exact physical placement, not the old image widget:
            // its float-to-i32 truncation can itself shift a pane by a pixel.
            let mut exact = tiny_skia::Pixmap::new(800, height).unwrap();
            exact.fill(tiny_skia::Color::BLACK);
            for row in 0..baseline.height as usize {
                for col in 0..baseline.width as usize {
                    let src = (row * baseline.width as usize + col) * 4;
                    let dst = ((row + offset as usize) * 800 + col + offset as usize) * 4;
                    let rgba = &baseline.rgba[src..src + 4];
                    exact.data_mut()[dst..dst + 4]
                        .copy_from_slice(&[rgba[2], rgba[1], rgba[0], rgba[3]]);
                }
            }
            assert!(
                exact.data() == bands.data(),
                "band seam at scale={scale} offset={offset}"
            );
        }
    }
}

#[test]
#[ignore = "release-only headless performance measurement"]
fn tiny_skia_frame_bench() {
    let mut expected = Vec::new();
    for banded in [false, true] {
        for all in [false, true] {
            let mut raster = Raster::new(2.5, 13.0, Cursor::Underline).unwrap();
            // Padded cells make the requested physical extent exact independently
            // of the installed font; glyph rendering still uses the real 2.5x font.
            raster.width = 25;
            raster.height = 50;
            let mut screen = Screen {
                cols: 90,
                rows: 25,
                cursor: (0, 12),
                cursor_visible: true,
                cells: (0..2250)
                    .map(|i| Cell {
                        c: char::from(b'!' + (i % 90) as u8),
                        fg: [210, 220, 230],
                        bg: [20, 25, 30],
                        bold: false,
                    })
                    .collect(),
                updated: Instant::now(),
            };
            let mut surface = Surface::default();
            let mut baseline = RgbaBand::default();
            let mut renderer = Renderer::new(Font::default(), Pixels(13.0));
            let viewport = Viewport::with_physical_size(Size::new(2250, 1250), 2.5);
            let bounds = Rectangle::with_size(viewport.logical_size());
            let mut targets: Vec<_> = (0..3)
                .map(|_| tiny_skia::Pixmap::new(2250, 1250).unwrap())
                .collect();
            let mut mask = tiny_skia::Mask::new(2250, 1250).unwrap();
            let mut history: std::collections::VecDeque<Vec<Layer>> = Default::default();
            let mut samples = Vec::new();
            let mut paint_ms = 0.0;
            let mut draw_ms = 0.0;
            let mut prepare_ms = 0.0;
            let mut area = 0.0;
            for n in 0..220 {
                let pixels = &mut targets[n as usize % 3];
                let mut dirty = vec![all; 25];
                dirty[12] = true;
                for (row, cells) in screen.cells.chunks_mut(90).enumerate() {
                    if dirty[row] {
                        for cell in cells {
                            cell.bg[0] = 20 + (n % 40) as u8;
                        }
                    }
                }
                let start = Instant::now();
                if banded {
                    surface.paint(&mut raster, &screen, &dirty);
                    surface.cache_handle(n);
                } else {
                    baseline.paint(&mut raster, &screen, &dirty);
                    baseline.cache_handle(n);
                }
                let painted = start.elapsed().as_secs_f64() * 1000.0;
                renderer.reset(bounds);
                let prepare_start = Instant::now();
                if banded {
                    widget::draw_images(
                        &mut renderer,
                        &surface.images(2.5),
                        iced::Point::ORIGIN,
                        2.5,
                        bounds,
                    );
                } else {
                    let handle = baseline.cached.as_ref().unwrap().1.clone();
                    let _ = renderer.measure_image(&handle);
                    let mut image = iced::advanced::image::Image::new(handle);
                    image.filter_method = image::FilterMethod::Nearest;
                    renderer.draw_image(image, bounds, bounds);
                }
                let prepared = prepare_start.elapsed().as_secs_f64() * 1000.0;
                let regions = history
                    .front()
                    .filter(|_| history.len() == 3)
                    .map(|old| {
                        damage::diff(
                            old,
                            renderer.layers(),
                            |layer| vec![layer.bounds],
                            Layer::damage,
                        )
                    })
                    .unwrap_or_else(|| vec![bounds]);
                let regions = damage::group(regions, bounds);
                history.push_back(renderer.layers().to_vec());
                if history.len() > 3 {
                    history.pop_front();
                }
                let draw_start = Instant::now();
                renderer.draw(
                    &mut pixels.as_mut(),
                    &mut mask,
                    &viewport,
                    &regions,
                    Color::BLACK,
                );
                let drawn = draw_start.elapsed().as_secs_f64() * 1000.0;
                if n >= 20 {
                    samples.push(start.elapsed().as_secs_f64() * 1000.0);
                    paint_ms += painted;
                    draw_ms += drawn;
                    prepare_ms += prepared;
                    area += regions
                        .iter()
                        .map(|r| (r.width * r.height * 6.25) as f64)
                        .sum::<f64>();
                }
                std::hint::black_box(pixels.data());
            }
            if banded {
                assert_eq!(
                    targets[219 % 3].data(),
                    expected[usize::from(all)],
                    "banded pixels differ from whole-pane baseline"
                );
            } else {
                expected.push(targets[219 % 3].data().to_vec());
            }
            samples.sort_by(f64::total_cmp);
            eprintln!(
                "{} {} 2250x1250 scale=2.5 age=3: mean={:.3} p50={:.3} p99={:.3} ms; paint+handle={:.3} prepare+convert={:.3} render={:.3} damaged_px={:.0}",
                if banded {
                    "native bands"
                } else {
                    "RGBA baseline"
                },
                if all { "redraw" } else { "echo" },
                samples.iter().sum::<f64>() / 200.0,
                samples[100],
                samples[198],
                paint_ms / 200.0,
                prepare_ms / 200.0,
                draw_ms / 200.0,
                area / 200.0
            );
        }
    }
}

/// Exercise the real layer diff against three rotating targets, and compare
/// every repaired frame to a fresh old RGBA+convert draw. No timing/Wayland.
#[test]
fn native_history_matches_rgba_with_clip_overlay_resize_and_age_loss() {
    for scale in [1.0, 1.25, 2.5] {
        let mut raster = Raster::new(scale, 13.0, Cursor::Block).unwrap();
        let mut screen = Screen {
            cols: 9,
            rows: 9,
            cursor: (2, 3),
            cursor_visible: true,
            cells: (0..81)
                .map(|i| Cell {
                    c: ['M', 'g', ' ', '@'][i % 4],
                    fg: [[255, 0, 127], [0, 255, 0], [0, 0, 255]][i % 3],
                    bg: [i as u8, 255 - i as u8, 31],
                    bold: i % 2 == 0,
                })
                .collect(),
            updated: Instant::now(),
        };
        let mut surface = Surface::default();
        let mut reference = RgbaBand::default();
        let viewport = Viewport::with_physical_size(Size::new(480, 480), scale);
        let full = Rectangle::with_size(viewport.logical_size());
        let mut renderer = Renderer::new(Font::default(), Pixels(13.0));
        let mut oracle = Renderer::new(Font::default(), Pixels(13.0));
        let mut mask = tiny_skia::Mask::new(480, 480).unwrap();
        let mut targets: Vec<_> = (0..3)
            .map(|_| tiny_skia::Pixmap::new(480, 480).unwrap())
            .collect();
        let mut history: std::collections::VecDeque<Vec<Layer>> = Default::default();
        let overlay = Handle::from_rgba(8, 8, [31, 47, 239, 127].repeat(64));
        for n in 0..12 {
            if n == 7 {
                screen.rows = 6;
                screen.cells.truncate(54); // Final band now has two rows.
            }
            if n == 9 {
                surface.invalidate();
            }
            let row = n % screen.rows;
            screen.cells[row * screen.cols].bg = [n as u8 * 19, 3, 201];
            screen.cursor = (2, row);
            screen.cursor_visible = n % 4 != 0;
            let mut dirty = vec![false; screen.rows];
            dirty[row] = true;
            surface.paint(&mut raster, &screen, &dirty);
            surface.cache_handle(n as u64);
            reference.paint(&mut raster, &screen, &[]);
            reference.cache_handle(n as u64);
            let origin = iced::Point::new(if n < 8 { 17.0 } else { 23.0 } / scale, 11.0 / scale);
            let bounds = Rectangle {
                x: origin.x,
                y: origin.y,
                width: reference.width as f32 / scale,
                height: reference.height as f32 / scale,
            };
            let clip = Rectangle {
                x: origin.x + 2.49 / scale,
                y: origin.y + 0.51 / scale,
                width: bounds.width - 5.0 / scale,
                height: bounds.height - 2.0 / scale,
            };
            for (r, native) in [(&mut renderer, true), (&mut oracle, false)] {
                r.reset(full);
                r.with_layer(clip, |r| {
                    if native {
                        widget::draw_images(r, &surface.images(scale), origin, scale, clip);
                    } else {
                        let mut image = iced::advanced::image::Image::new(
                            reference.cached.as_ref().unwrap().1.clone(),
                        );
                        image.filter_method = image::FilterMethod::Nearest;
                        r.draw_image(image, bounds, clip);
                    }
                    // Same image sublayer: an overlapping translucent image
                    // after the grid must remain above it, including repair.
                    if n % 3 != 0 {
                        r.draw_image(
                            iced::advanced::image::Image::new(overlay.clone()),
                            Rectangle {
                                x: origin.x + 4.0 / scale,
                                y: origin.y + 4.0 / scale,
                                width: 8.0 / scale,
                                height: 8.0 / scale,
                            },
                            clip,
                        );
                    }
                });
            }
            // Unknown age must discard the damaged target's old contents.
            if n == 5 {
                history.clear();
                targets[n % 3].fill(tiny_skia::Color::from_rgba8(255, 0, 255, 255));
            }
            let regions = history
                .front()
                .filter(|_| history.len() == 3)
                .map(|old| damage::diff(old, renderer.layers(), |l| vec![l.bounds], Layer::damage))
                .unwrap_or_else(|| vec![full]);
            let regions = damage::group(regions, full);
            history.push_back(renderer.layers().to_vec());
            if history.len() > 3 {
                history.pop_front();
            }
            renderer.draw(
                &mut targets[n % 3].as_mut(),
                &mut mask,
                &viewport,
                &regions,
                Color::BLACK,
            );
            let mut expected = tiny_skia::Pixmap::new(480, 480).unwrap();
            oracle.draw(
                &mut expected.as_mut(),
                &mut mask,
                &viewport,
                &[full],
                Color::BLACK,
            );
            assert_eq!(
                targets[n % 3].data(),
                expected.data(),
                "scale={scale} frame={n}"
            );
        }
    }
}

/// Isolated costs, not additive frame timings. No window/presentation is
/// created. Keep allocations outside the timer except where explicitly named.
#[test]
#[ignore = "release-only rank-6 warm-paint measurement"]
fn raster_warm_spans_bench() {
    use cosmix_term_core::raster::PaintState;
    use std::hint::black_box;

    let mut raster = Raster::new(2.5, 13.0, Cursor::Underline).unwrap();
    raster.width = 25;
    raster.height = 50;
    for spaces in [false, true] {
        for run_cells in [90, 7, 1] {
            let mut screen = Screen {
                cols: 90,
                rows: 25,
                cursor: (0, 0),
                cursor_visible: false,
                cells: (0..2250)
                    .map(|i| Cell {
                        c: if spaces { ' ' } else { char::from(b'!' + (i % 90) as u8) },
                        fg: [210, 220, 230],
                        bg: [20, 25, 30],
                        bold: false,
                    })
                    .collect(),
                updated: Instant::now(),
            };
            let mut pixels = vec![0; 2250 * 1250 * 4];
            let mut state = PaintState::default();
            let mut samples = Vec::with_capacity(200);
            for n in 0..220 {
                // Change real content without changing the warmed glyph keys.
                // Set up the colours outside the timed region.
                for (i, cell) in screen.cells.iter_mut().enumerate() {
                    cell.bg[0] = 20 + (n % 40) as u8
                        + (((i % 90) / run_cells) % 2) as u8;
                }
                let start = Instant::now();
                black_box(raster.paint(
                    black_box(&screen), &mut pixels, 2250 * 4, &mut state, &[true; 25],
                ));
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                black_box(&pixels);
                if n >= 20 {
                    samples.push(ms);
                }
            }
            samples.sort_by(f64::total_cmp);
            eprintln!(
                "rank6 warm {} bg_run={run_cells} cells 2250x1250 scale=2.5: mean={:.3} p50={:.3} p99={:.3} ms",
                if spaces { "spaces" } else { "glyphs" },
                samples.iter().sum::<f64>() / samples.len() as f64,
                samples[100],
                samples[198],
            );
        }
    }
}

/// Isolated costs, not additive frame timings. No window/presentation is
/// created. Keep allocations outside the timer except where explicitly named.
#[test]
#[ignore = "release-only headless performance measurement"]
fn tiny_skia_foot_phases_bench() {
    use cosmix_term_core::raster::PaintState;
    use std::hint::black_box;

    fn measure(label: &str, mut work: impl FnMut()) {
        let mut samples = Vec::new();
        for n in 0..220 {
            let start = Instant::now();
            work();
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            if n >= 20 {
                samples.push(ms);
            }
        }
        samples.sort_by(f64::total_cmp);
        eprintln!(
            "phase {label}: mean={:.3} p50={:.3} p99={:.3} ms",
            samples.iter().sum::<f64>() / samples.len() as f64,
            samples[100],
            samples[198]
        );
    }

    let mut raster = Raster::new(2.5, 13.0, Cursor::Underline).unwrap();
    raster.width = 25;
    raster.height = 50;
    let mut screen = Screen {
        cols: 90,
        rows: 25,
        cursor: (0, 12),
        cursor_visible: false,
        cells: (0..2250)
            .map(|i| Cell {
                c: char::from(b'!' + (i % 90) as u8),
                fg: [210, 220, 230],
                bg: [20, 25, 30],
                bold: false,
            })
            .collect(),
        updated: Instant::now(),
    };
    let mut rgba = vec![0; 2250 * 1250 * 4];
    let mut state = PaintState::default();
    for echo in [true, false] {
        let mut dirty = vec![!echo; 25];
        dirty[12] = true;
        measure(
            if echo {
                "paint warm echo (90 cells)"
            } else {
                "paint warm full (2250 cells)"
            },
            || {
                raster.paint(&screen, &mut rgba, 2250 * 4, &mut state, &dirty);
                black_box(&rgba);
            },
        );
    }
    measure("paint cold full + new raster (90 distinct keys)", || {
        let mut cold = raster.resized(2.5, 13.0).unwrap();
        cold.width = 25;
        cold.height = 50;
        cold.paint(&screen, &mut rgba, 2250 * 4, &mut state, &[]);
        black_box(&rgba);
    });
    for cell in &mut screen.cells {
        cell.c = ' ';
    }
    measure("paint spaces full (background only)", || {
        raster.paint(&screen, &mut rgba, 2250 * 4, &mut state, &[]);
        black_box(&rgba);
    });

    // Same loop as iced_tiny_skia::raster::Cache::allocate, with the load,
    // allocation and id lookup excluded. Alpha is opaque, as in production.
    let mut native = tiny_skia::Pixmap::new(2250, 1250).unwrap();
    measure("RGBA to native premultiplied BGRA loop full", || {
        for (src, dst) in black_box(&rgba).chunks_exact(4).zip(native.pixels_mut()) {
            *dst = tiny_skia::ColorU8::from_rgba(src[2], src[1], src[0], src[3]).premultiply();
        }
        black_box(native.data());
    });
    let mut target = tiny_skia::Pixmap::new(2250, 1250).unwrap();
    for rows in [50, 200, 1250] {
        measure(&format!("native copy {rows} pixel rows"), || {
            let len = 2250 * rows * 4;
            target.data_mut()[..len].copy_from_slice(black_box(&native.data()[..len]));
            black_box(target.data());
        });
    }
    measure("native scroll copy_within 24 rows", || {
        target.data_mut().copy_within(2250 * 50 * 4.., 0);
        black_box(target.data());
    });
    measure("tiny-skia draw_pixmap identity full", || {
        target.draw_pixmap(
            0,
            0,
            native.as_ref(),
            &tiny_skia::PixmapPaint::default(),
            tiny_skia::Transform::identity(),
            None,
        );
        black_box(target.data());
    });
    // Exercise the real image-cache miss, including image::load and allocation.
    let mut renderer = Renderer::new(Font::default(), Pixels(13.0));
    let viewport = Viewport::with_physical_size(Size::new(2250, 1250), 2.5);
    let bounds = Rectangle::with_size(viewport.logical_size());
    let mut mask = tiny_skia::Mask::new(2250, 1250).unwrap();
    let pixels = Bytes::from(rgba);
    measure(
        "measure_image new id full (load + allocation + convert)",
        || {
            let handle = Handle::from_rgba(2250, 1250, pixels.clone());
            black_box(renderer.measure_image(&handle));
            // Empty damage still trims the image cache; avoid an unbounded cache
            // without charging a draw to the conversion measurement.
            renderer.draw(
                &mut target.as_mut(),
                &mut mask,
                &viewport,
                &[],
                Color::BLACK,
            );
        },
    );
    let handle = Handle::from_rgba(2250, 1250, pixels);
    renderer.reset(bounds);
    black_box(renderer.measure_image(&handle));
    let mut image = iced::advanced::image::Image::new(handle.clone());
    image.filter_method = image::FilterMethod::Nearest;
    renderer.draw_image(image, bounds, bounds);
    measure("iced cached image draw full", || {
        black_box(renderer.measure_image(&handle));
        renderer.draw(
            &mut target.as_mut(),
            &mut mask,
            &viewport,
            &[bounds],
            Color::BLACK,
        );
        black_box(target.data());
    });
    renderer.reset(bounds);
    measure("iced empty layer clear full", || {
        renderer.draw(
            &mut target.as_mut(),
            &mut mask,
            &viewport,
            &[bounds],
            Color::BLACK,
        );
        black_box(target.data());
    });
}
