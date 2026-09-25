//! Primary face discovery only. Unicode/emoji fallback stays in unicode_raster.
use cosmix_design::{TypographyGeneric, TypographyRole, default_typography};
use fontdb::{Database, Family, Query, Source, Style, Weight};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use swash::FontRef;

const LEGACY_PATHS: &[&str] = &[
    "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/liberation/LiberationMono-Regular.ttf",
    "/usr/share/fonts/truetype/liberation2/LiberationMono-Regular.ttf",
    "/usr/share/fonts/noto/NotoSansMono-Regular.ttf",
    "/usr/share/fonts/truetype/noto/NotoSansMono-Regular.ttf",
    "/usr/share/fonts/TTF/JetBrainsMono-Regular.ttf",
];

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
    let mut db = Database::new();
    db.load_system_fonts();
    from_database(&mut db)
        .or_else(|| {
            LEGACY_PATHS
                .iter()
                .find_map(|path| from_path(Path::new(path)).ok())
        })
        .ok_or_else(|| "No monospace font found; set TERM_SPIKE_FONT=/path/to/font.ttf".into())
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
    let query = Query {
        families,
        weight: Weight(default_typography(TypographyRole::Terminal).weight),
        style: Style::Normal,
        ..Query::default()
    };
    while let Some(id) = db.query(&query) {
        let loaded = db.face_source(id).and_then(|(source, index)| {
            let path = match source {
                Source::File(path) | Source::SharedFile(path, _) => path,
                Source::Binary(_) => return None,
            };
            let data: Arc<[u8]> = std::fs::read(&path).ok()?.into();
            FontRef::from_index(&data, index as usize)?;
            Some(Primary { path, index, data })
        });
        if loaded.is_some() {
            return loaded;
        }
        // A stale path or a face Swash cannot load must not hide the next
        // usable face/family in the ordered query.
        db.remove_face(id);
    }
    None
}

#[cfg(any(test, feature = "test-support"))]
pub(super) fn fixture() -> Result<Primary, String> {
    LEGACY_PATHS[..2]
        .iter()
        .find_map(|path| from_path(Path::new(path)).ok())
        .ok_or_else(|| "raster fixtures require DejaVu Sans Mono".into())
}
