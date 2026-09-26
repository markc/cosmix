use super::{
    Raster,
    primary_font::{self, Primary},
    tests::screen,
};
use crate::config::{Config, Cursor};
use fontdb::{Database, Family, Query, Weight};
use std::{path::Path, process::Command};
use swash::{FontDataRef, FontRef, StringId};

fn loaded_family(font: FontRef<'_>) -> String {
    let strings = font.localized_strings();
    strings
        .find_by_id(StringId::TypographicFamily, None)
        .or_else(|| strings.find_by_id(StringId::Family, None))
        .expect("loaded face family")
        .chars()
        .collect()
}

fn assert_face(primary: &Primary, raster: &Raster, family: &str, weight: u16) {
    let font = raster.unicode.fonts.primary.font();
    assert_eq!(loaded_family(font), family);
    assert_eq!(font.attributes().weight().0, weight);
    assert_eq!(font.attributes().style(), swash::Style::Normal);
    // Inspect the actual loaded face, not just the discovery request.
    let index = FontDataRef::new(&primary.data)
        .unwrap()
        .fonts()
        .position(|face| face.offset == font.offset)
        .unwrap();
    assert_eq!(index, primary.index as usize);
    let metrics = font.metrics(&[]).scale(raster.px);
    assert_eq!(
        raster.width,
        font.glyph_metrics(&[])
            .scale(raster.px)
            .advance_width(font.charmap().map('M'))
            .ceil()
            .max(1.0) as u32
    );
    assert_eq!(
        raster.height,
        (metrics.ascent + metrics.descent.abs() + metrics.leading)
            .ceil()
            .max(1.0) as u32
    );
    assert_eq!(raster.baseline, metrics.ascent.ceil() as i32);
}

fn raster(primary: &Primary) -> Raster {
    Raster::from_font(
        primary.data.clone(),
        primary.index,
        2.5,
        Config::default().font_px,
        Cursor::Underline,
    )
    .unwrap()
}

fn write_row(primary: &Primary, name: &str, family: &str, weight: u16) {
    let mut raster = raster(primary);
    assert_face(primary, &raster, family, weight);
    assert_face(
        primary,
        &raster.resized(1.25, 21.333).unwrap(),
        family,
        weight,
    );
    let text = "term $ printf 'Hello, SF Mono! 0123456789'";
    let mut row = screen(text.len(), 1, ' ');
    for (cell, c) in row.cells.iter_mut().zip(text.chars()) {
        cell.c = c;
    }
    let pixels = raster.render(&row);
    assert!(
        pixels.chunks_exact(4).any(|p| p[..3] != [0, 0, 0]),
        "empty row"
    );
    let (width, height) = raster.target_size(&row);
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/font-probes");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("term-row-{name}-2.5x.png"));
    let mut encoder = png::Encoder::new(std::fs::File::create(&path).unwrap(), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .unwrap()
        .write_image_data(&pixels)
        .unwrap();
    println!(
        "{} (font={}, face={}, family={family}, weight={weight})",
        path.display(),
        primary.path.display(),
        primary.index
    );
}

#[test]
fn discovers_sf_mono_light_and_renders_row_at_2_5x() {
    let mut db = Database::new();
    db.load_system_fonts();
    if db
        .query(&Query {
            families: &[Family::Name("SF Mono")],
            ..Query::default()
        })
        .is_none()
    {
        eprintln!("SKIP SF Mono Light probe: fontdb cannot find SF Mono");
        return;
    }
    let primary = primary_font::from_database(&mut db).expect("terminal primary face");
    write_row(&primary, "sf", "SF Mono", 300);
}

#[test]
fn excludes_sf_and_renders_dejavu_row_at_2_5x() {
    let mut db = Database::new();
    db.load_system_fonts();
    let excluded: Vec<_> = db
        .faces()
        .filter(|face| {
            face.families
                .iter()
                .any(|(name, _)| name.starts_with("SF "))
        })
        .map(|face| face.id)
        .collect();
    for id in excluded {
        db.remove_face(id);
    }
    // Deliberately call only database selection: no direct-path rescue can
    // reintroduce an excluded SF face. Do not alter installed font files.
    let primary = primary_font::from_database(&mut db).expect("free terminal primary face");
    write_row(&primary, "free", "DejaVu Sans Mono", 400);
}

#[test]
#[ignore = "subprocess fixture for term_spike_font_wins"]
fn override_child() {
    // Populate the system cache first: an override must still win afterwards.
    let first = primary_font::discover(None).unwrap();
    let second = primary_font::discover(None).unwrap();
    assert!(std::sync::Arc::ptr_eq(&first.data, &second.data));
    let raster = Raster::new(1.0, Config::default().font_px, Cursor::Underline).unwrap();
    let primary = primary_font::fixture().unwrap();
    assert_eq!(raster.data.as_ref(), primary.data.as_ref());
    assert_face(&primary, &raster, "DejaVu Sans Mono", 400);
}

#[test]
fn term_spike_font_wins() {
    // No global env mutation: other discovery/oracle tests can run in parallel.
    let primary = primary_font::fixture().unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "raster::primary_font_tests::override_child",
            "--ignored",
            "--nocapture",
        ])
        .env("TERM_SPIKE_FONT", &primary.path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    assert!(primary_font::discover(Some(Path::new("/nonexistent/term-font.ttf"))).is_err());
}

#[test]
fn collection_face_index_survives_metrics_ascii_unicode_and_resize() {
    let regular = primary_font::fixture().unwrap();
    let bold = primary_font::fixture_weight(Weight::BOLD).unwrap();
    // Build a temporary TTC from installed free fonts, with Regular at index 1.
    // Table offsets in a TTC are absolute from the collection, not the face.
    let mut collection = b"ttcf\0\x01\0\0\0\0\0\x02\0\0\0\0\0\0\0\0".to_vec();
    for (index, source) in [&bold, &regular].into_iter().enumerate() {
        while !collection.len().is_multiple_of(4) {
            collection.push(0);
        }
        let base = collection.len() as u32;
        collection[12 + index * 4..16 + index * 4].copy_from_slice(&base.to_be_bytes());
        // A discovered fixture may itself live in a collection. Extract only
        // its SFNT directory and tables, relocating all offsets into this TTC.
        let font = FontRef::from_index(&source.data, source.index as usize).unwrap();
        let bytes = &source.data;
        let offset = font.offset as usize;
        let tables = u16::from_be_bytes(bytes[offset + 4..offset + 6].try_into().unwrap()) as usize;
        let mut face = bytes[offset..offset + 12 + tables * 16].to_vec();
        for table in 0..tables {
            let at = 12 + table * 16 + 8;
            let offset = u32::from_be_bytes(face[at..at + 4].try_into().unwrap());
            let len = u32::from_be_bytes(face[at + 4..at + 8].try_into().unwrap());
            while !face.len().is_multiple_of(4) {
                face.push(0);
            }
            let relocated = base + face.len() as u32;
            face[at..at + 4].copy_from_slice(&relocated.to_be_bytes());
            face.extend_from_slice(&bytes[offset as usize..(offset + len) as usize]);
        }
        collection.extend(face);
    }
    let path =
        std::env::temp_dir().join(format!("term-font-collection-{}.ttc", std::process::id()));
    std::fs::write(&path, collection).unwrap();
    let mut db = Database::new();
    db.load_font_file(&path).unwrap();
    let primary = primary_font::from_database(&mut db).unwrap();
    assert_eq!(primary.index, 1);
    assert_eq!(primary_font::from_path(&path).unwrap().index, 1);
    let mut actual = raster(&primary);
    let mut expected = raster(&regular);
    assert_face(&primary, &actual, "DejaVu Sans Mono", 400);
    let mut row = screen(3, 1, 'M');
    row.cells[1].c = 'é';
    assert_eq!(actual.render(&row), expected.render(&row));
    let mut resized = actual.resized(1.25, 21.333).unwrap();
    let mut reference = expected.resized(1.25, 21.333).unwrap();
    assert_face(&primary, &resized, "DejaVu Sans Mono", 400);
    assert_eq!(resized.render(&row), reference.render(&row));
    // Both test-only paths must also retain Regular at collection index 1.
    actual.unicode = super::UnicodeRaster::new(super::Fonts::without_fallbacks(
        actual.data.clone(),
        actual.unicode.fonts.primary.index,
    ));
    assert_eq!(actual.render(&row), expected.render(&row));
    let (width, height) = actual.target_size(&row);
    let mut pixels = vec![0; width as usize * height as usize * 4];
    actual.cache.clear();
    actual.paint_reference(
        &row,
        &mut pixels,
        width as usize * 4,
        &mut super::PaintState::default(),
        &[],
        super::PixelFormat::Rgba,
    );
    assert_eq!(pixels, expected.render(&row));
    std::fs::remove_file(path).unwrap();
}

/// Point each named table's directory entry at EOF with zero length.
fn moved_to_eof(data: &[u8], offset: usize, tables: usize, tags: &[&[u8; 4]]) -> Vec<u8> {
    let mut out = data.to_vec();
    let end = (out.len() as u32).to_be_bytes();
    for table in 0..tables {
        let at = offset + 12 + table * 16;
        if tags.iter().any(|tag| out[at..at + 4] == tag[..]) {
            out[at + 8..at + 12].copy_from_slice(&end);
            out[at + 12..at + 16].copy_from_slice(&0_u32.to_be_bytes());
        }
    }
    out
}

#[test]
fn damaged_optional_face_is_skipped_for_the_next_path() {
    // Emoji/symbols faces load index 0 of a plain font file, like the fixture.
    let fixture = primary_font::fixture().unwrap();
    assert_eq!(fixture.index, 0);
    let tables = u16::from_be_bytes(fixture.data[4..6].try_into().unwrap()) as usize;
    let damaged = moved_to_eof(&fixture.data, 0, tables, &[b"maxp"]);
    assert_ne!(damaged[..], fixture.data[..]);
    let dir = std::env::temp_dir().join(format!("term-font-optional-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (bad, good) = (dir.join("no-maxp.ttf"), dir.join("good.ttf"));
    std::fs::write(&bad, &damaged).unwrap();
    std::fs::write(&good, &fixture.data[..]).unwrap();
    let face = super::unicode::optional(&[bad.to_str().unwrap(), good.to_str().unwrap()])
        .expect("the intact face after the damaged one");
    assert_eq!(face.font().data, &fixture.data[..]);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn damaged_candidate_is_removed_before_selecting_next_face() {
    let fixture = primary_font::fixture().unwrap();
    let dir = std::env::temp_dir().join(format!("term-font-damaged-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("damaged.ttf");
    let mut source_db = Database::new();
    source_db.load_font_file(&fixture.path).unwrap();
    let template = source_db
        .faces()
        .find(|face| face.index == fixture.index)
        .unwrap()
        .clone();
    // Retain stale discovery metadata, as with a damaged file after scanning.
    // The truncated directory passes FontRef::from_index; the missing outlines
    // case retains good metrics, so validation must actually render glyphs.
    let font = FontRef::from_index(&fixture.data, fixture.index as usize).unwrap();
    let offset = font.offset as usize;
    let tables =
        u16::from_be_bytes(fixture.data[offset + 4..offset + 6].try_into().unwrap()) as usize;
    let truncated = fixture.data[..offset + 12 + tables * 16].to_vec();
    assert!(FontRef::from_index(&truncated, fixture.index as usize).is_some());
    let no_outlines = moved_to_eof(&fixture.data, offset, tables, &[b"glyf", b"CFF ", b"CFF2"]);
    // head, hhea and hmtx intact but maxp past EOF: swash's metrics proxy
    // stops before counting long metrics, so advance_width must never be asked.
    let no_maxp = moved_to_eof(&fixture.data, offset, tables, &[b"maxp"]);
    for damaged in [b"not a font".to_vec(), truncated, no_outlines, no_maxp] {
        std::fs::write(&path, damaged).unwrap();
        let mut db = Database::new();
        let mut bad = template.clone();
        bad.source = fontdb::Source::File(path.clone());
        bad.weight = Weight::LIGHT;
        let bad_id = db.push_face_info(bad);
        db.push_face_info(template.clone());
        let primary = primary_font::from_database(&mut db).unwrap();
        assert_eq!(primary.path, fixture.path);
        assert!(db.face(bad_id).is_none());
    }
    std::fs::remove_file(path).unwrap();
    std::fs::remove_dir(dir).unwrap();
}
