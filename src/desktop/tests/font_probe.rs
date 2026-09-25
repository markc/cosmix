// Shared CPU shaping/raster acceptance harness for comp and Quoin. Only free
// fonts are fixtures; licensed SF faces must come from the host's discovery.
use bevy::prelude::*;
use bevy::text::{ComputedTextBlock, FontCx, LayoutCx, TextBounds, TextPipeline};

pub fn sf_installed() -> bool {
    if std::path::Path::new("/usr/share/fonts/apple-fonts").is_dir() {
        true
    } else {
        eprintln!("SKIP SF font acceptance: /usr/share/fonts/apple-fonts is absent");
        false
    }
}

pub fn free_fonts_only() -> FontCx {
    let mut fonts = FontCx::default();
    fonts.collection = fontique::Collection::new(fontique::CollectionOptions {
        shared: false,
        system_fonts: false,
    });
    fonts.collection.register_fonts(
        Font::from_bytes(
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../crates/cosmix-comp/assets/fonts/DejaVuSans.ttf"
            ))
            .to_vec(),
        )
        .data,
        None,
    );
    let fira = fonts.collection.register_fonts(
        Font::from_bytes(bevy::text::DEFAULT_FONT_DATA.to_vec()).data,
        None,
    )[0]
    .0;
    // Deliberately different generic: the explicit free chain must win.
    fonts
        .collection
        .set_generic_families(fontique::GenericFamily::SansSerif, [fira].into_iter());
    fonts
        .collection
        .set_generic_families(fontique::GenericFamily::SystemUi, [fira].into_iter());
    fonts
        .collection
        .set_generic_families(fontique::GenericFamily::Monospace, [fira].into_iter());
    for family in ["SF Pro Text", "SF Pro Display", "SF Mono"] {
        assert!(
            fonts.collection.family_id(family).is_none(),
            "SF is excluded from this collection"
        );
    }
    fonts
}

/// Shape the consumer's reconciled TextFont through Bevy, inspect every actual
/// run's bytes/index, then rasterise those same glyphs with Bevy's Swash backend.
pub fn assert_face_and_render(
    fonts: &mut FontCx,
    font: &TextFont,
    sample: &str,
    expected_family: &str,
    expected_weight: u16,
    scale: f32,
    filename: &str,
) {
    let mut block = ComputedTextBlock::default();
    TextPipeline::default()
        .update_buffer(
            &Assets::default(),
            std::iter::once((
                Entity::PLACEHOLDER,
                0,
                sample,
                font,
                Color::WHITE,
                Default::default(),
                Default::default(),
            )),
            Default::default(),
            Default::default(),
            TextBounds::UNBOUNDED,
            scale,
            &mut block,
            fonts,
            &mut LayoutCx::default(),
            Vec2::new(1600.0, 400.0),
            16.0,
        )
        .expect("consumer font shapes");
    let layout = block.buffer();
    let mut count = 0;
    for line in layout.lines() {
        for run in line.runs() {
            let selected = run.font();
            let face = ttf_parser::Face::parse(selected.data.as_ref(), selected.index)
                .expect("actual shaped face parses at its selected collection index");
            let names: Vec<_> = face.names().into_iter().collect();
            let family = [
                ttf_parser::name_id::TYPOGRAPHIC_FAMILY,
                ttf_parser::name_id::FAMILY,
            ]
            .into_iter()
            .find_map(|id| {
                names
                    .iter()
                    .find_map(|name| (name.name_id == id).then(|| name.to_string()).flatten())
            })
            .expect("shaped face has a family name");
            assert_eq!(
                family, expected_family,
                "{filename}: actual face, not requested family"
            );
            assert_eq!(
                face.weight().to_number(),
                expected_weight,
                "{filename}: actual OS/2 weight"
            );
            assert!(!face.is_italic(), "desktop default is upright");
            if let bevy::text::FontSize::Px(px) = font.font_size {
                assert!((run.font_size() - px * scale).abs() < 0.001);
            }
            eprintln!(
                "{filename}: {family}, weight {}, face index {}, {}px",
                face.weight().to_number(),
                selected.index,
                run.font_size()
            );
            count += 1;
        }
    }
    assert!(count > 0, "must inspect at least one shaped face");

    let mut image = image::RgbaImage::from_pixel(
        (layout.width().ceil() as u32 + 32).max(64),
        (layout.height().ceil() as u32 + 32).max(64),
        image::Rgba([24, 24, 28, 255]),
    );
    let mut cache = swash::scale::ScaleContext::new();
    let mut ink = 0;
    for line in layout.lines() {
        for item in line.items() {
            let parley::PositionedLayoutItem::GlyphRun(glyphs) = item else {
                continue;
            };
            let run = glyphs.run();
            let selected = run.font();
            let face = swash::FontRef::from_index(selected.data.as_ref(), selected.index as usize)
                .expect("Swash supports the selected font, including SF's CFF outlines");
            let mut scaler = cache
                .builder(face)
                .size(run.font_size())
                .normalized_coords(run.normalized_coords())
                .build();
            for glyph in glyphs.positioned_glyphs() {
                assert_ne!(glyph.id, 0, "sample must not contain tofu");
                let Some(bitmap) = swash::scale::Render::new(&[swash::scale::Source::Outline])
                    .format(swash::zeno::Format::Alpha)
                    .render(&mut scaler, glyph.id as u16)
                else {
                    continue;
                }; // spaces have no outline
                for y in 0..bitmap.placement.height {
                    for x in 0..bitmap.placement.width {
                        let alpha = bitmap.data[(y * bitmap.placement.width + x) as usize] as u32;
                        let px = 16 + glyph.x.round() as i32 + bitmap.placement.left + x as i32;
                        let py = 16 + glyph.y.round() as i32 - bitmap.placement.top + y as i32;
                        if alpha > 0
                            && px >= 0
                            && py >= 0
                            && (px as u32) < image.width()
                            && (py as u32) < image.height()
                        {
                            let pixel = image.get_pixel_mut(px as u32, py as u32);
                            for channel in &mut pixel.0[..3] {
                                *channel =
                                    ((*channel as u32 * (255 - alpha) + 240 * alpha) / 255) as u8;
                            }
                            ink += 1;
                        }
                    }
                }
            }
        }
    }
    assert!(ink > 0, "sample must render visible glyphs");
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"));
    let path = target
        .join("font-probes")
        .join(format!("{filename}-{scale}x.png"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    image.save(&path).unwrap();
    eprintln!("Font sample: {}", path.display());
}
