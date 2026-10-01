//! Installed font registration and named Material glyphs for iced consumers.
//! Registration uses iced's shared font system before theme family selection.
//! Material variable axes beyond weight remain renderer dependent.

use cosmix_assets::AssetSet;
use iced_core::{Font, font};
use iced_graphics::text::font_system;
use std::{
    borrow::Cow,
    collections::HashSet,
    sync::{Mutex, OnceLock},
};

static INSTALLED: OnceLock<Result<Option<AssetSet>, String>> = OnceLock::new();

/// Load the complete verified set once. Absence is a normal platform fallback;
/// malformed or unreadable installed assets are reported to the caller.
pub fn register_installed() -> Result<Option<&'static AssetSet>, &'static str> {
    match INSTALLED.get_or_init(|| {
        let set = AssetSet::discover().map_err(|error| error.to_string())?;
        if let Some(set) = &set {
            // Read every face before mutating the renderer's collection.
            let data = set
                .font_paths()
                .into_iter()
                .map(|path| {
                    std::fs::read(&path)
                        .map_err(|error| format!("font {}: {error}", path.display()))
                })
                .collect::<Result<Vec<_>, _>>()?;
            // Identify families from the selected bytes, including old sets
            // whose manifest predates family metadata. The selected release
            // owns these families; keeping a preloaded system face with the
            // same name would let fontdb query that earlier source instead.
            let mut selected = iced_graphics::text::cosmic_text::fontdb::Database::new();
            for bytes in &data {
                selected.load_font_data(bytes.clone());
            }
            let families: HashSet<_> = selected
                .faces()
                .flat_map(|face| face.families.iter())
                .map(|(name, _)| name.to_ascii_lowercase())
                .collect();
            for role in ["sans", "mono", "serif", "icons", "emoji"] {
                if let Some(family) = set.family(role)
                    && !families.contains(&family.to_ascii_lowercase())
                {
                    return Err(format!(
                        "installed {role} font family {family:?} could not be registered"
                    ));
                }
            }
            drop(selected);
            let mut system = font_system()
                .write()
                .map_err(|_| "iced font system lock poisoned".to_owned())?;
            let conflicts: Vec<_> = system
                .raw()
                .db()
                .faces()
                .filter(|face| {
                    face.families
                        .iter()
                        .any(|(name, _)| families.contains(&name.to_ascii_lowercase()))
                })
                .map(|face| face.id)
                .collect();
            // db_mut invalidates cosmic-text's family-match cache; load_font
            // also increments iced's version so existing paragraphs refresh.
            for id in conflicts {
                system.raw().db_mut().remove_face(id);
            }
            for bytes in data {
                system.load_font(Cow::Owned(bytes));
            }
            for role in ["sans", "mono", "serif", "icons", "emoji"] {
                if let Some(family) = set.family(role)
                    && !system.raw().db().faces().any(|face| {
                        face.families
                            .iter()
                            .any(|(name, _)| name.eq_ignore_ascii_case(family))
                    })
                {
                    return Err(format!(
                        "installed {role} font family {family:?} could not be registered"
                    ));
                }
            }
        }
        Ok(set)
    }) {
        Ok(set) => Ok(set.as_ref()),
        Err(error) => {
            static REPORTED: OnceLock<()> = OnceLock::new();
            REPORTED.get_or_init(|| eprintln!("iced static assets: {error}"));
            Err(error.as_str())
        }
    }
}

/// Resolve an authored family chain. An untouched embedded role can prefer
/// the installed sans/mono family; explicit design families retain precedence.
pub fn font_for(
    family: &str,
    fallbacks: &[String],
    requested_weight: u16,
    monospace: bool,
    prefer_assets: bool,
) -> Font {
    let set = register_installed().ok().flatten();
    let preferred = prefer_assets
        .then(|| set.and_then(|set| set.family(if monospace { "mono" } else { "sans" })))
        .flatten();
    let names: Vec<_> = preferred
        .into_iter()
        .chain(std::iter::once(family))
        .chain(fallbacks.iter().map(String::as_str))
        .collect();
    let (installed, has_light) = {
        let mut system = font_system().write().expect("font system");
        let db = system.raw().db();
        let found = names.iter().find(|name| {
            db.faces().any(|face| {
                face.families
                    .iter()
                    .any(|(family, _)| family.eq_ignore_ascii_case(name))
            })
        });
        let light = found.is_some_and(|name| {
            db.faces().any(|face| {
                face.weight.0 == 300
                    && face
                        .families
                        .iter()
                        .any(|(family, _)| family.eq_ignore_ascii_case(name))
            })
        });
        (found.map(|name| (*name).to_owned()), light)
    };
    let family = match installed {
        Some(name) => font::Family::Name(intern(&name)),
        None if monospace => font::Family::Monospace,
        None => font::Family::SansSerif,
    };
    let weight = match cosmix_design::family_font_weight(requested_weight, has_light) {
        0..=150 => font::Weight::Thin,
        151..=250 => font::Weight::ExtraLight,
        251..=350 => font::Weight::Light,
        351..=450 => font::Weight::Normal,
        451..=550 => font::Weight::Medium,
        551..=650 => font::Weight::Semibold,
        651..=750 => font::Weight::Bold,
        751..=850 => font::Weight::ExtraBold,
        _ => font::Weight::Black,
    };
    Font {
        family,
        weight,
        ..Font::DEFAULT
    }
}

/// The codepoint and explicitly named font, ready for a text widget.
/// Absence/unknown names return None; a broken installed set returns an error.
pub fn material_icon(name: &str) -> Result<Option<(char, Font)>, &'static str> {
    let Some(set) = register_installed()? else {
        return Ok(None);
    };
    Ok(set
        .icon(name)
        .zip(set.family("icons"))
        .map(|(glyph, family)| {
            (
                glyph,
                Font {
                    family: font::Family::Name(intern(family)),
                    ..Font::DEFAULT
                },
            )
        }))
}

fn intern(name: &str) -> &'static str {
    static NAMES: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let mut names = NAMES
        .get_or_init(Default::default)
        .lock()
        .expect("font names");
    if let Some(existing) = names.get(name) {
        return existing;
    }
    let leaked: &'static str = Box::leak(name.to_owned().into_boxed_str());
    names.insert(leaked);
    leaked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires a bootstrapped static asset set"]
    fn installed_roles_and_material_glyph_resolve() {
        use iced_graphics::text::cosmic_text::fontdb::{Database, Family, Language, Query};
        let expected = AssetSet::discover().unwrap().expect("installed set");
        let sans_bytes = std::fs::read(expected.font_path("sans").unwrap()).unwrap();
        let mono_bytes = std::fs::read(expected.font_path("mono").unwrap()).unwrap();
        assert_ne!(sans_bytes, mono_bytes);
        // A preloaded conflicting family points at different valid font bytes.
        // Only fontdb's in-memory family metadata is changed, never a font file.
        let mut source = Database::new();
        source.load_font_data(mono_bytes);
        let mut conflict = source.faces().next().unwrap().clone();
        conflict.families = vec![(
            expected.family("sans").unwrap().to_owned(),
            Language::English_UnitedStates,
        )];
        let stale_id = {
            let mut system = font_system().write().unwrap();
            system.raw().db_mut().push_face_info(conflict)
        };
        let set = register_installed().unwrap().expect("installed set");
        {
            let mut system = font_system().write().unwrap();
            let db = system.raw().db();
            assert!(
                db.face(stale_id).is_none(),
                "conflicting system face removed"
            );
            let id = db
                .query(&Query {
                    families: &[Family::Name(set.family("sans").unwrap())],
                    ..Default::default()
                })
                .unwrap();
            assert_eq!(
                db.with_face_data(id, |bytes, _| bytes.to_vec()).unwrap(),
                sans_bytes
            );
        }
        let sans = font_for("Missing family", &[], 400, false, true);
        let mono = font_for("Missing family", &[], 400, true, true);
        assert_eq!(
            sans.family,
            font::Family::Name(intern(set.family("sans").unwrap()))
        );
        assert_eq!(
            mono.family,
            font::Family::Name(intern(set.family("mono").unwrap()))
        );
        let (glyph, font) = material_icon("delete").unwrap().unwrap();
        assert_eq!(glyph, '\u{e872}');
        assert_eq!(
            font.family,
            font::Family::Name(intern(set.family("icons").unwrap()))
        );
        assert!(material_icon("not_a_material_icon").unwrap().is_none());
    }
}
