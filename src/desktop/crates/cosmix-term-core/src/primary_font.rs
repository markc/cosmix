//! Validated terminal face discovery. Fallback routing stays in unicode_raster.
use cosmix_design::{TypographyGeneric, TypographyRole, default_typography};
use fontdb::{Database, Family, Query, Source, Style, Weight};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};
use swash::{
    FontRef,
    scale::{Render, ScaleContext, Source as GlyphSource},
    zeno::Format,
};

const LEGACY_PATHS: &[&str] = &[
    "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/liberation/LiberationMono-Regular.ttf",
    "/usr/share/fonts/truetype/liberation2/LiberationMono-Regular.ttf",
    "/usr/share/fonts/noto/NotoSansMono-Regular.ttf",
    "/usr/share/fonts/truetype/noto/NotoSansMono-Regular.ttf",
    "/usr/share/fonts/TTF/JetBrainsMono-Regular.ttf",
];

#[derive(Clone)]
pub(super) struct Primary {
    pub path: PathBuf,
    pub index: u32,
    pub data: Arc<[u8]>,
}

pub(super) fn discover(override_path: Option<&Path>) -> Result<Primary, String> {
    // An explicit path is authoritative, including an error for a bad file.
    if let Some(path) = override_path {
        return from_path(path);
    }
    static SHARED: OnceLock<Result<Primary, String>> = OnceLock::new();
    SHARED
        .get_or_init(|| {
            let mut db = Database::new();
            db.load_system_fonts();
            from_database(&mut db)
                .or_else(|| {
                    LEGACY_PATHS
                        .iter()
                        .find_map(|path| from_path(Path::new(path)).ok())
                })
                .ok_or_else(|| {
                    "No monospace font found; set TERM_SPIKE_FONT=/path/to/font.ttf".into()
                })
        })
        .clone()
}

/// Called only when the shared Unicode fallback set is first needed.
pub(super) fn coverage() -> Vec<Primary> {
    let mut db = Database::new();
    db.load_system_fonts();
    default_typography(TypographyRole::Terminal)
        .fallbacks
        .iter()
        .filter_map(|name| select(&mut db, &[Family::Name(name)]))
        .collect()
}

pub(super) fn from_database(db: &mut Database) -> Option<Primary> {
    let role = default_typography(TypographyRole::Terminal);
    let mut families: Vec<_> = std::iter::once(&role.family)
        .chain(&role.fallbacks)
        .map(|name| Family::Name(name.as_str()))
        .collect();
    families.push(match role.generic {
        TypographyGeneric::Monospace => Family::Monospace,
        TypographyGeneric::SansSerif => Family::SansSerif,
    });
    select(db, &families)
}

pub(super) fn from_path(path: &Path) -> Result<Primary, String> {
    let mut db = Database::new();
    db.load_font_file(path)
        .map_err(|e| format!("font {}: {e}; set TERM_SPIKE_FONT", path.display()))?;
    // Even overrides can be collections. Match Light/normal within this file,
    // rather than losing its chosen index when constructing either renderer.
    let names: Vec<_> = db
        .faces()
        .flat_map(|face| face.families.iter())
        .map(|(name, _)| name.clone())
        .collect();
    let families: Vec<_> = names.iter().map(|name| Family::Name(name)).collect();
    select(&mut db, &families).ok_or_else(|| {
        format!(
            "Invalid font {}; set TERM_SPIKE_FONT to a TTF/OTF font",
            path.display()
        )
    })
}

fn select(db: &mut Database, families: &[Family<'_>]) -> Option<Primary> {
    select_weight(
        db,
        families,
        Weight(default_typography(TypographyRole::Terminal).weight),
    )
}

fn select_weight(db: &mut Database, families: &[Family<'_>], weight: Weight) -> Option<Primary> {
    for family in families {
        while let Some(id) = query_family(db, family, weight) {
            let loaded = db.face_source(id).and_then(|(source, index)| {
                let path = match source {
                    Source::File(path) | Source::SharedFile(path, _) => path,
                    Source::Binary(_) => return None,
                };
                let data: Arc<[u8]> = std::fs::read(&path).ok()?.into();
                let font = FontRef::from_index(&data, index as usize)?;
                if !usable(font) {
                    return None;
                }
                Some(Primary { path, index, data })
            });
            if loaded.is_some() {
                return loaded;
            }
            // A stale path or a face Swash cannot load must not hide the next
            // usable face/family in the ordered query.
            db.remove_face(id);
        }
    }
    None
}

fn query_family(db: &mut Database, family: &Family<'_>, weight: Weight) -> Option<fontdb::ID> {
    let mut query = Query {
        families: std::slice::from_ref(family),
        weight,
        style: Style::Normal,
        ..Query::default()
    };
    loop {
        let id = db.query(&query)?;
        if db.face(id)?.weight >= weight {
            return Some(id);
        }
        // CSS searches below 300 before considering 400. Retry this family
        // (including a fontconfig generic alias) at Regular, never Thin.
        if query.weight < Weight::NORMAL {
            query.weight = Weight::NORMAL;
        } else {
            db.remove_face(id);
        }
    }
}

fn usable(font: FontRef<'_>) -> bool {
    if font.metrics(&[]).units_per_em == 0 {
        return false;
    }
    let metrics = font.glyph_metrics(&[]);
    let mut context = ScaleContext::new();
    let mut scaler = context.builder(font).size(24.0).hint(true).build();
    ['M', '0'].into_iter().all(|ch| {
        let glyph = font.charmap().map(ch);
        let advance = metrics.advance_width(glyph);
        glyph != 0
            && advance.is_finite()
            && advance > 0.0
            && Render::new(&[GlyphSource::Outline])
                .format(Format::Alpha)
                .render(&mut scaler, glyph)
                .is_some_and(|image| {
                    image.placement.width > 0
                        && image.placement.height > 0
                        && image.data.iter().any(|sample| *sample != 0)
                })
    })
}

#[cfg(any(test, feature = "test-support"))]
pub(super) fn fixture() -> Result<Primary, String> {
    static SHARED: OnceLock<Result<Primary, String>> = OnceLock::new();
    SHARED
        .get_or_init(|| fixture_weight(Weight::NORMAL))
        .clone()
}

#[cfg(any(test, feature = "test-support"))]
pub(super) fn fixture_weight(weight: Weight) -> Result<Primary, String> {
    let mut db = Database::new();
    db.load_system_fonts();
    select_weight(&mut db, &[Family::Name("DejaVu Sans Mono")], weight)
        .or_else(|| {
            LEGACY_PATHS[..2].iter().find_map(|path| {
                let path = if weight == Weight::BOLD {
                    Path::new(path).with_file_name("DejaVuSansMono-Bold.ttf")
                } else {
                    PathBuf::from(path)
                };
                from_path(&path).ok()
            })
        })
        .ok_or_else(|| "raster fixtures require DejaVu Sans Mono".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn light_queries_reject_thin_and_extra_light_even_via_generic_alias() {
        let fixture = fixture().unwrap();
        let mut source = Database::new();
        source.load_font_file(&fixture.path).unwrap();
        let template = source.faces().next().unwrap().clone();
        for weights in [
            &[100, 400][..],
            &[200, 400],
            &[100, 200, 400],
            &[100, 200, 300, 400],
        ] {
            for family in [Family::Name("Test Mono"), Family::Monospace] {
                let mut db = Database::new();
                db.set_monospace_family("Test Mono");
                for weight in weights {
                    let mut face = template.clone();
                    face.families =
                        vec![("Test Mono".into(), fontdb::Language::English_UnitedStates)];
                    face.style = Style::Normal;
                    face.weight = Weight(*weight);
                    db.push_face_info(face);
                }
                let id = query_family(&mut db, &family, Weight::LIGHT).unwrap();
                let expected = if weights.contains(&300) { 300 } else { 400 };
                assert_eq!(db.face(id).unwrap().weight, Weight(expected));
            }
        }
    }
}
