use super::tests::{raster_with, screen};
use super::*;
use crate::{clusters::ClusterInterner, config::Cursor, terminal::Terminal};

/// Full-frame oracle with no damage, span expansion, production compositing,
/// or warm cluster-cache reuse. Swash/font selection is shared as the glyph
/// backend; independent fixtures below check shaping and source alpha too.
pub(super) fn paint_emoji_reference(
    raster: &Raster,
    screen: &Screen,
    dst: &mut [u8],
    stride: usize,
    format: PixelFormat,
) {
    let rows = screen.rows.min(screen.cells.len() / screen.cols.max(1));
    let cw = raster.width as usize;
    let ch = raster.height as usize;
    let order = |rgb: [u8; 3]| match format {
        PixelFormat::Rgba => rgb,
        PixelFormat::Bgra => [rgb[2], rgb[1], rgb[0]],
    };
    for y in 0..rows * ch {
        for x in 0..screen.cols * cw {
            let bg = order(screen.cells[y / ch * screen.cols + x / cw].bg);
            dst[y * stride + x * 4..y * stride + x * 4 + 4]
                .copy_from_slice(&[bg[0], bg[1], bg[2], 255]);
        }
    }
    let mut backend = UnicodeRaster::new(raster.unicode.fonts.clone());
    let mut context = ScaleContext::new();
    let mut ascii = HashMap::new();
    for row in 0..rows {
        for col in 0..screen.cols {
            let cell = screen.cells[row * screen.cols + col];
            if matches!(cell.width, CellWidth::Spacer | CellWidth::LeadingSpacer)
                || (cell.extra == 0 && matches!(cell.c, ' ' | '\0'))
            {
                continue;
            }
            let span = if cell.width == CellWidth::Wide
                && col + 1 < screen.cols
                && screen.cells[row * screen.cols + col + 1].width == CellWidth::Spacer
            {
                2
            } else {
                1
            };
            let fg = order(cell.fg);
            if cell.c.is_ascii() && cell.extra == 0 && cell.width == CellWidth::Narrow {
                let glyph = ascii.entry(cell.c).or_insert_with(|| {
                    let font = FontRef::from_index(&raster.data, 0).unwrap();
                    Render::new(&[Source::Outline])
                        .format(Format::Alpha)
                        .render(
                            &mut context.builder(font).size(raster.px).hint(true).build(),
                            font.charmap().map(cell.c),
                        )
                });
                if let Some(glyph) = glyph {
                    for y in 0..ch {
                        for x in 0..cw {
                            let gx = x as i64 - i64::from(glyph.placement.left);
                            let gy = y as i64 - i64::from(raster.baseline)
                                + i64::from(glyph.placement.top);
                            if gx < 0
                                || gy < 0
                                || gx >= i64::from(glyph.placement.width)
                                || gy >= i64::from(glyph.placement.height)
                            {
                                continue;
                            }
                            let a = u32::from(
                                glyph.data
                                    [gy as usize * glyph.placement.width as usize + gx as usize],
                            );
                            let at = (row * ch + y) * stride + (col * cw + x) * 4;
                            for c in 0..3 {
                                dst[at + c] = ((u32::from(fg[c]) * a
                                    + u32::from(dst[at + c]) * (255 - a))
                                    / 255) as u8;
                            }
                        }
                    }
                }
                continue;
            }
            let mut scalar = [0; 4];
            let text = if cell.extra == 0 {
                cell.c.encode_utf8(&mut scalar)
            } else {
                screen.clusters.get(cell.extra).unwrap_or("")
            };
            let image = backend.image(
                text,
                span,
                raster.px,
                (raster.width, raster.height),
                raster.baseline,
            );
            // Destination-driven loop, deliberately unlike paint_cluster's
            // clipped source slices. Overlap and differing backgrounds matter.
            for y in 0..ch {
                for x in 0..cw * span {
                    let at = (row * ch + y) * stride + (col * cw + x) * 4;
                    for layer in &image.layers {
                        let sx = x as i64 - i64::from(layer.x);
                        let sy = y as i64 - i64::from(layer.y);
                        if sx < 0
                            || sy < 0
                            || sx >= i64::from(layer.width)
                            || sy >= i64::from(layer.height)
                        {
                            continue;
                        }
                        let i = sy as usize * layer.width as usize + sx as usize;
                        match &layer.pixels {
                            Pixels::Mask(mask) => {
                                let a = u32::from(mask[i]);
                                for c in 0..3 {
                                    dst[at + c] = ((u32::from(fg[c]) * a
                                        + u32::from(dst[at + c]) * (255 - a))
                                        / 255)
                                        as u8;
                                }
                            }
                            Pixels::Color(data) => {
                                let rgb = order([data[i * 4], data[i * 4 + 1], data[i * 4 + 2]]);
                                let a = u32::from(data[i * 4 + 3]);
                                for c in 0..3 {
                                    dst[at + c] = (u32::from(rgb[c])
                                        + u32::from(dst[at + c]) * (255 - a) / 255)
                                        .min(255)
                                        as u8;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    let (mut col, row) = screen.cursor;
    if screen.cursor_visible && col < screen.cols && row < rows {
        if col > 0
            && screen.cells[row * screen.cols + col].width == CellWidth::Spacer
            && screen.cells[row * screen.cols + col - 1].width == CellWidth::Wide
        {
            col -= 1;
        }
        let span = if screen.cells[row * screen.cols + col].width == CellWidth::Wide
            && col + 1 < screen.cols
            && screen.cells[row * screen.cols + col + 1].width == CellWidth::Spacer
        {
            2
        } else {
            1
        };
        for y in row * ch..(row + 1) * ch {
            if raster.cursor == Cursor::Underline && y + 1 != (row + 1) * ch {
                continue;
            }
            for x in col * cw..(col + span) * cw {
                let at = y * stride + x * 4;
                for c in 0..3 {
                    dst[at + c] = if raster.cursor == Cursor::Block {
                        255 - dst[at + c]
                    } else {
                        220
                    };
                }
            }
        }
    }
}

pub(super) fn transition(grid: &mut Screen, dirty: &mut [bool], value: u64, frame: usize) {
    if grid.cols < 2 {
        return;
    }
    let rows = paintable_rows(grid);
    if rows == 0 {
        return;
    }
    let row = value as usize % rows;
    let col = (value >> 16) as usize % (grid.cols - 1);
    let i = row * grid.cols + col;
    let mut interner = ClusterInterner::default();
    // Keep all texts in stable slots across transitions. A fresh generation
    // would force a full repaint and hide the pair-damage regression.
    if grid.clusters.get(1).is_none() {
        for text in ["👩‍💻", "🇦🇺", "👍🏽", "❤️", "e\u{301}", "👍🏻"] {
            interner.intern(text);
        }
        grid.clusters = interner.snapshot;
    }
    dirty[row] = true;
    match frame % 6 {
        0 | 1 | 4 => {
            let id = 1 + ((value >> 32) % 6) as u32;
            let c = grid.clusters.get(id).unwrap().chars().next().unwrap();
            grid.cells[i].c = c;
            grid.cells[i].extra = id;
            grid.cells[i].width = if id == 5 {
                CellWidth::Narrow
            } else {
                CellWidth::Wide
            };
            grid.cells[i + 1].c = ' ';
            grid.cells[i + 1].extra = 0;
            grid.cells[i + 1].width = if id == 5 {
                CellWidth::Narrow
            } else {
                CellWidth::Spacer
            };
        }
        2 => {
            for c in &mut grid.cells[i..i + 2] {
                c.c = 'a';
                c.extra = 0;
                c.width = CellWidth::Narrow;
            }
        }
        3 => grid.cells[i + 1].bg = [value as u8, 11, 99],
        _ => {
            grid.cursor = (col + 1, row);
            grid.cursor_visible = true;
        }
    }
}

#[test]
fn noto_french_flag_golden_regions_and_two_cell_clip() {
    // Independent golden: the installed Noto French flag is blue/white/red,
    // read left-to-right. Independently rasterised with ImageMagick/Pango's
    // font_desc="Noto Color Emoji 109", its quarter/centre/three-quarter samples are (0,40,153),
    // (255,255,255), (235,36,51). No UnicodeRaster::image/reference oracle.
    // Allow strike shading/resampling, but reject tofu, swapped channels,
    // monochrome masks, wrong placement, or painting outside the VT box.
    let mut raster = raster_with(Cursor::Block).resized(2.0, 13.0).unwrap();
    raster.width = 20;
    raster.height = 32;
    let mut grid = Terminal::from_test_vt(6, 3, "\r\n  🇫🇷".as_bytes()).screen(false);
    grid.cursor_visible = false;
    for cell in &mut grid.cells {
        cell.bg = [17, 29, 43];
    }
    let pixels = raster.render(&grid);
    let stride = 6 * 20 * 4;
    let mean = |cx: usize| -> [u32; 3] {
        let mut sum = [0; 3];
        for y in 46..50 {
            for x in cx - 1..=cx + 1 {
                for c in 0..3 {
                    sum[c] += u32::from(pixels[y * stride + x * 4 + c]);
                }
            }
        }
        sum.map(|v| v / 12)
    };
    let blue = mean(50);
    let white = mean(60);
    let red = mean(70);
    for (actual, golden) in [
        (blue, [0, 40, 153]),
        (white, [255; 3]),
        (red, [235, 36, 51]),
    ] {
        for c in 0..3 {
            assert!(
                actual[c].abs_diff(golden[c]) <= 20,
                "{actual:?} != {golden:?}"
            );
        }
    }
    assert!(
        blue[2] >= 70 && blue[2] > blue[0] + 40 && blue[2] > blue[1] + 20,
        "{blue:?}"
    );
    assert!(white.iter().all(|c| *c >= 200), "{white:?}");
    assert!(
        red[0] >= 150 && red[1] < 100 && red[2] + 60 < red[0],
        "{red:?}"
    );
    for y in 0..96 {
        for x in 0..120 {
            if !(40..80).contains(&x) || !(32..64).contains(&y) {
                assert_eq!(&pixels[y * stride + x * 4..][..4], &[17, 29, 43, 255]);
            }
        }
    }
}

#[test]
fn noto_clusters_ligate_and_paint_colour_in_both_halves() {
    for scale in [1.0, 1.25, 1.5, 2.5] {
        let mut raster = raster_with(Cursor::Block).resized(scale, 13.0).unwrap();
        for text in ["👩‍💻", "🇦🇺", "👍🏽", "❤️"] {
            let term = Terminal::from_test_vt(5, 2, format!(" {text}").as_bytes());
            let mut grid = term.screen(false);
            grid.cursor_visible = false;
            grid.cells[1].bg = [3, 13, 23];
            grid.cells[2].bg = [73, 53, 33];
            let image = raster.unicode.image(
                text,
                2,
                raster.px,
                (raster.width, raster.height),
                raster.baseline,
            );
            assert_eq!(
                image.glyphs, 1,
                "Noto fixture must ligate {text}; install Noto Color Emoji"
            );
            assert!(
                image
                    .layers
                    .iter()
                    .any(|l| matches!(l.pixels, Pixels::Color(_))),
                "colour fixture required"
            );
            for format in [PixelFormat::Rgba, PixelFormat::Bgra] {
                let (width, height) = raster.target_size(&grid);
                let stride = width as usize * 4 + 7;
                let mut dst = vec![0x5a; stride * height as usize + 13];
                let mut expected = dst.clone();
                raster.paint_format(
                    &grid,
                    &mut dst,
                    stride,
                    &mut PaintState::default(),
                    &[],
                    format,
                );
                paint_emoji_reference(&raster, &grid, &mut expected, stride, format);
                assert_eq!(dst, expected, "{text} scale={scale} {format:?}");
                for col in [1, 2] {
                    let bg = format.colour(grid.cells[col].bg);
                    let mut chromatic = false;
                    for y in 0..raster.height as usize {
                        for x in col * raster.width as usize..(col + 1) * raster.width as usize {
                            let p = &dst[y * stride + x * 4..][..3];
                            chromatic |= p != bg && (p[0] != p[1] || p[1] != p[2]);
                        }
                    }
                    assert!(chromatic, "{text} did not colour half {col}");
                }
                for y in 0..height as usize {
                    for x in 0..width as usize {
                        if y < raster.height as usize
                            && (raster.width as usize..3 * raster.width as usize).contains(&x)
                        {
                            continue;
                        }
                        let bg = format.colour(
                            grid.cells[y / raster.height as usize * grid.cols
                                + x / raster.width as usize]
                                .bg,
                        );
                        assert_eq!(&dst[y * stride + x * 4..][..4], &[bg[0], bg[1], bg[2], 255]);
                    }
                    assert!(
                        dst[y * stride + width as usize * 4..(y + 1) * stride]
                            .iter()
                            .all(|v| *v == 0x5a)
                    );
                }
            }
        }
    }
}

#[test]
fn pair_damage_cursor_extras_selection_and_breakup_match_full_oracle() {
    for style in [Cursor::Block, Cursor::Underline] {
        for format in [PixelFormat::Rgba, PixelFormat::Bgra] {
            let mut raster = raster_with(style);
            let mut grid = Terminal::from_test_vt(6, 2, " 👍🏽 ".as_bytes()).screen(false);
            let mut interner = ClusterInterner::default();
            let dark = interner.intern("👍🏽");
            let light = interner.intern("👍🏻");
            grid.clusters = interner.snapshot;
            grid.cells[1].extra = dark;
            grid.cursor_visible = false;
            let stride = grid.cols * raster.width as usize * 4 + 3;
            let mut pixels = vec![0x5a; stride * grid.rows * raster.height as usize];
            let mut state = PaintState::default();
            for step in 0..9 {
                let mut dirty = [false; 2];
                match step {
                    1 => {
                        grid.cells[1].extra = light;
                        dirty[0] = true;
                    }
                    2 => {
                        grid.cells[2].bg = [10, 30, 70];
                        dirty[0] = true;
                    }
                    3 => {
                        for c in &mut grid.cells[1..3] {
                            std::mem::swap(&mut c.fg, &mut c.bg);
                        }
                        dirty[0] = true;
                    }
                    4 => {
                        grid.cursor = (2, 0);
                        grid.cursor_visible = true;
                    }
                    5 => grid.cursor = (1, 0), // same normalised cursor: no damage
                    6 => grid.cursor_visible = false,
                    7 => {
                        for c in &mut grid.cells[1..3] {
                            c.c = 'x';
                            c.width = CellWidth::Narrow;
                            c.extra = 0;
                        }
                        dirty[0] = true;
                    }
                    8 => {
                        grid.cells[5].c = '界';
                        grid.cells[5].width = CellWidth::Wide;
                        dirty[0] = true;
                    }
                    _ => {}
                }
                let bands = raster
                    .paint_format(&grid, &mut pixels, stride, &mut state, &dirty, format)
                    .to_vec();
                let mut expected = vec![0x5a; pixels.len()];
                paint_emoji_reference(&raster, &grid, &mut expected, stride, format);
                assert_eq!(pixels, expected, "step={step} {style:?} {format:?}");
                if step == 5 {
                    assert!(bands.is_empty());
                } else if (1..=7).contains(&step) {
                    assert_eq!(
                        bands,
                        [DamageBand {
                            x: raster.width,
                            width: raster.width * 2,
                            y: 0,
                            height: raster.height
                        }]
                    );
                }
            }
        }
    }
}

#[test]
fn missing_fonts_and_invalid_ids_have_bounded_tofu_and_negative_cache() {
    let mut raster = raster_with(Cursor::Block);
    raster.unicode = UnicodeRaster::new(Fonts::without_fallbacks(raster.data.clone()));
    let mut grid = screen(4, 2, ' ');
    grid.cells[1].c = '\u{10ffff}';
    grid.cells[1].width = CellWidth::Wide;
    grid.cells[2].width = CellWidth::Spacer;
    for extra in [0, crate::clusters::MISSING_CLUSTER] {
        grid.cells[1].extra = extra;
        let first = raster.render(&grid);
        assert!(first.chunks_exact(4).any(|p| p[..3] == grid.cells[1].fg));
        let misses = raster.unicode.misses;
        assert_eq!(raster.render(&grid), first);
        assert_eq!(raster.unicode.misses, misses, "negative results are cached");
        let mut expected = vec![0; first.len()];
        paint_emoji_reference(
            &raster,
            &grid,
            &mut expected,
            grid.cols * raster.width as usize * 4,
            PixelFormat::Rgba,
        );
        assert_eq!(first, expected);
    }
}

#[test]
fn variation_selector_width_changes_and_shifted_pair_edges_expand_damage() {
    let mut raster = raster_with(Cursor::Block);
    let mut grid = screen(7, 2, ' ');
    let mut interner = ClusterInterner::default();
    let heart = interner.intern("❤️");
    grid.clusters = interner.snapshot;
    grid.cells[1].c = '❤';
    let mut surface = Surface::default();
    raster.render_into(&grid, &[], &mut surface);
    grid.cells[1].extra = heart;
    grid.cells[1].width = CellWidth::Wide;
    grid.cells[2].width = CellWidth::Spacer;
    assert_eq!(
        raster.render_into(&grid, &[true, false], &mut surface),
        &[DamageBand {
            x: raster.width,
            y: 0,
            width: raster.width * 2,
            height: raster.height,
        }]
    );
    // Shift a pair by one: the old spacer becomes the new lead. Both graphs
    // must be expanded before old cells are overwritten (three columns).
    grid.cells[2] = grid.cells[1];
    grid.cells[1].c = ' ';
    grid.cells[1].extra = 0;
    grid.cells[1].width = CellWidth::Narrow;
    grid.cells[3].width = CellWidth::Spacer;
    assert_eq!(
        raster.render_into(&grid, &[true, false], &mut surface),
        &[DamageBand {
            x: raster.width,
            y: 0,
            width: raster.width * 3,
            height: raster.height,
        }]
    );
    grid.cells[2].extra = 0;
    grid.cells[2].width = CellWidth::Narrow;
    grid.cells[3].width = CellWidth::Narrow;
    raster.render_into(&grid, &[true, false], &mut surface);
    let mut expected = vec![0; surface.rgba().len()];
    paint_emoji_reference(
        &raster,
        &grid,
        &mut expected,
        surface.stride(),
        PixelFormat::Rgba,
    );
    assert_eq!(surface.rgba(), expected);
}

#[test]
fn wrapped_leading_spacer_is_background_only() {
    let mut raster = raster_with(Cursor::Block);
    let mut grid = Terminal::from_test_vt(4, 3, "abc👩‍💻".as_bytes()).screen(false);
    grid.cursor_visible = false;
    assert_eq!(grid.cells[3].width, CellWidth::LeadingSpacer);
    grid.cells[3].bg = [10, 30, 70];
    let pixels = raster.render(&grid);
    let stride = grid.cols * raster.width as usize * 4;
    for y in 0..raster.height as usize {
        for x in 3 * raster.width as usize..4 * raster.width as usize {
            assert_eq!(&pixels[y * stride + x * 4..][..4], &[10, 30, 70, 255]);
        }
    }
    let mut expected = vec![0; pixels.len()];
    paint_emoji_reference(&raster, &grid, &mut expected, stride, PixelFormat::Rgba);
    assert_eq!(pixels, expected);
}

#[test]
fn a_space_base_does_not_hide_its_combining_mark() {
    let mut raster = raster_with(Cursor::Block);
    let mut grid = Terminal::from_test_vt(4, 2, " \u{301}".as_bytes()).screen(false);
    grid.cursor_visible = false;
    let image = raster.unicode.image(
        " \u{301}",
        1,
        raster.px,
        (raster.width, raster.height),
        raster.baseline,
    );
    assert!(
        image.glyphs > 0,
        "space plus accent must shape rather than become tofu"
    );
    let painted = raster.render(&grid);
    grid.cells[0].extra = 0;
    let blank = raster.render(&grid);
    assert_ne!(
        painted, blank,
        "combining mark was skipped with its space base"
    );
}

#[test]
fn interner_generation_invalidates_reused_ids_but_append_does_not() {
    let mut raster = raster_with(Cursor::Block);
    let mut grid = screen(4, 1, ' ');
    let mut interner = ClusterInterner::default();
    grid.cells[0].c = 'e';
    grid.cells[0].extra = interner.intern("e\u{301}");
    grid.clusters = interner.snapshot.clone();
    let mut surface = Surface::default();
    raster.render_into(&grid, &[], &mut surface);
    interner.intern("e\u{302}");
    grid.clusters = interner.snapshot;
    assert!(raster.is_current(&grid, &surface.state, &[false], PixelFormat::Rgba));
    let old = surface.rgba().to_vec();
    let mut new = ClusterInterner::default();
    assert_eq!(new.intern("e\u{302}"), grid.cells[0].extra);
    grid.clusters = new.snapshot;
    assert!(!raster.is_current(&grid, &surface.state, &[false], PixelFormat::Rgba));
    assert!(!raster.render_into(&grid, &[false], &mut surface).is_empty());
    assert_ne!(surface.rgba(), old);
}

#[test]
#[ignore = "cluster release benchmark; run serially with --nocapture"]
fn cached_ascii_and_emoji_paint_benchmark() {
    use std::time::Instant;
    let mut raster = raster_with(Cursor::Block).resized(2.5, 13.0).unwrap();
    for text in ["M".repeat(90), "👩‍💻".repeat(45), "┌─┬┐│└┴┘⣿⠿".repeat(9)]
    {
        let mut grid = Terminal::from_test_vt(90, 25, text.repeat(25).as_bytes()).screen(false);
        grid.cursor_visible = false;
        let mut surface = Surface::default();
        raster.render_into(&grid, &[], &mut surface);
        let misses = raster.unicode.misses;
        let mut elapsed = Vec::new();
        for frame in 0..220 {
            for cell in &mut grid.cells {
                cell.bg = [frame as u8, 20, 30];
            }
            let at = Instant::now();
            raster.render_into(&grid, &[true; 25], &mut surface);
            if frame >= 20 {
                elapsed.push(at.elapsed().as_secs_f64() * 1000.0);
            }
        }
        elapsed.sort_by(f64::total_cmp);
        assert_eq!(
            raster.unicode.misses, misses,
            "warm paint must not shape or resample"
        );
        eprintln!(
            "{} {}x{} mean={:.3}ms p50={:.3}ms p99={:.3}ms",
            text.chars().next().unwrap(),
            surface.width(),
            surface.height(),
            elapsed.iter().sum::<f64>() / elapsed.len() as f64,
            elapsed[100],
            elapsed[198]
        );
    }
}
