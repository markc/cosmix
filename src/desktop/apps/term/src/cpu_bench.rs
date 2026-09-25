//! Offscreen equivalent of compositor::present, including layer history and
//! damage grouping. Unlike Headless::screenshot this retains the target and
//! does not add a full-window screenshot allocation and BGRA→RGBA readback.
use super::*;
use cosmix_term_core::{config::Cursor, terminal::Cell};
use iced::advanced::{Renderer as _, image::Renderer as _};
use iced::{Color, Font, Pixels, Rectangle, Size};
use iced_tiny_skia::{
    Layer, Renderer,
    graphics::{Viewport, damage},
};
use std::time::Instant;

#[test]
fn band_widget_matches_whole_image_at_fractional_scale_and_offset() {
    for (scale, cell_height) in [
        (1.0, 20),
        (1.1, 41),
        (1.25, 20),
        (1.5, 20),
        (1.75, 41),
        (2.25, 20),
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
        let mut baseline = PixelBand::default();
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
            let bounds = Rectangle {
                x: origin.x,
                y: origin.y,
                width: baseline.width as f32 / scale,
                height: baseline.height as f32 / scale,
            };
            let mut renderer = Renderer::new(Font::default(), Pixels(13.0));
            let mut mask = tiny_skia::Mask::new(800, height).unwrap();
            let mut whole = tiny_skia::Pixmap::new(800, height).unwrap();
            renderer.reset(clip);
            let mut image =
                iced::advanced::image::Image::new(baseline.cached.as_ref().unwrap().1.clone());
            image.filter_method = image::FilterMethod::Nearest;
            renderer.draw_image(image, bounds, clip);
            renderer.draw(
                &mut whole.as_mut(),
                &mut mask,
                &viewport,
                &[clip],
                Color::BLACK,
            );
            renderer.reset(clip);
            widget::draw_images(&mut renderer, &surface.images(scale), origin, scale, clip);
            let mut bands = tiny_skia::Pixmap::new(800, height).unwrap();
            renderer.draw(
                &mut bands.as_mut(),
                &mut mask,
                &viewport,
                &[clip],
                Color::BLACK,
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
            let mut baseline = PixelBand::default();
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
                if banded { "bands" } else { "baseline" },
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
