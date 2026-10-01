//! Register the verified, installed font set in an existing Bevy collection.
//! The application retains ownership of its generic families and fallback policy.

use bevy_text::FontCx;
pub use cosmix_assets::AssetSet;
use std::sync::OnceLock;

/// One pinned selection shared by every Bevy consumer in the process.
pub fn installed_set() -> Result<Option<&'static AssetSet>, &'static str> {
    static INSTALLED: OnceLock<Result<Option<AssetSet>, String>> = OnceLock::new();
    match INSTALLED.get_or_init(|| AssetSet::discover().map_err(|error| error.to_string())) {
        Ok(set) => Ok(set.as_ref()),
        Err(error) => Err(error.as_str()),
    }
}

/// One registration per font context. Retains the pinned set for role lookup.
#[derive(Clone, Debug, Default)]
pub struct RegisteredAssets {
    set: Option<AssetSet>,
}

impl RegisteredAssets {
    pub fn discover_and_register(fonts: &mut FontCx) -> Result<Self, String> {
        let set = installed_set()?.cloned();
        if let Some(set) = &set {
            register_set(fonts, set);
            for role in ["sans", "mono", "serif", "icons", "emoji"] {
                if let Some(family) = set.family(role)
                    && !selected_path_matches(fonts, set, role, family)
                {
                    return Err(format!(
                        "installed {role} font family {family:?} could not be registered"
                    ));
                }
            }
        }
        Ok(Self { set })
    }

    pub fn family(&self, role: &str) -> Option<&str> {
        self.set.as_ref()?.family(role)
    }

    /// Bevy clears its whole collection when a font asset is removed. Restore
    /// installed faces after that event without resolving a different set.
    pub fn register_missing(&self, fonts: &mut FontCx) {
        let Some(set) = &self.set else {
            return;
        };
        if ["sans", "mono", "serif", "icons", "emoji"]
            .into_iter()
            .any(|role| {
                set.family(role)
                    .is_some_and(|family| !selected_path_matches(fonts, set, role, family))
            })
        {
            register_set(fonts, set);
        }
    }

    pub fn icon(&self, name: &str) -> Option<char> {
        self.set.as_ref()?.icon(name)
    }
}

fn selected_path_matches(fonts: &mut FontCx, set: &AssetSet, role: &str, family: &str) -> bool {
    let Some(path) = set.font_path(role) else {
        return false;
    };
    fonts.collection.family_by_name(family).and_then(|family| family.default_font().map(|font| font.source().kind().clone()))
        .is_some_and(|source| matches!(source, fontique::SourceKind::Path(selected) if selected.as_ref() == path.as_path()))
}

fn register_set(fonts: &mut FontCx, set: &AssetSet) {
    // Earlier explicitly registered faces of an identical family/style can
    // otherwise win Fontique's matching. Handle aliases remain separate.
    for role in ["sans", "mono", "serif", "icons", "emoji"] {
        if let Some(name) = set.family(role)
            && let Some(family) = fonts.collection.family_by_name(name)
        {
            let id = family.id();
            let attributes: Vec<_> = family
                .fonts()
                .iter()
                .map(|font| (font.width(), font.style(), font.weight()))
                .collect();
            for (width, style, weight) in attributes {
                fonts.collection.unregister_font(id, width, style, weight);
            }
        }
    }
    fonts.collection.load_fonts_from_paths(set.font_paths());
}

#[cfg(test)]
mod tests {
    use super::*;

    // This gate uses a real bootstrapped set supplied through COSMIX_SHARE.
    // It deliberately disables system discovery: success must come from the set.
    #[test]
    #[ignore = "requires a bootstrapped static asset set"]
    fn installed_set_registers_without_system_fonts() {
        let mut fonts = FontCx::default();
        fonts.context.collection = fontique::Collection::new(fontique::CollectionOptions {
            system_fonts: false,
            ..Default::default()
        });
        let assets = RegisteredAssets::discover_and_register(&mut fonts).unwrap();
        for role in ["sans", "mono", "serif", "icons", "emoji"] {
            let family = assets.family(role).expect("manifest family");
            assert!(
                fonts.collection.family_id(family).is_some(),
                "{role}: {family}"
            );
            assert!(selected_path_matches(
                &mut fonts,
                assets.set.as_ref().unwrap(),
                role,
                family
            ));
        }
        assert_eq!(assets.icon("delete"), Some('\u{e872}'));
        fonts.collection.clear();
        // Simulate a family surviving in an external/system collection: names
        // alone are present, but all point at the wrong source bytes.
        let data = fontique::Blob::from(
            std::fs::read(assets.set.as_ref().unwrap().font_path("sans").unwrap()).unwrap(),
        );
        for role in ["sans", "mono", "serif", "icons", "emoji"] {
            fonts.collection.register_fonts(
                data.clone(),
                Some(fontique::FontInfoOverride {
                    family_name: assets.family(role),
                    ..Default::default()
                }),
            );
        }
        assets.register_missing(&mut fonts);
        for role in ["sans", "mono", "serif", "icons", "emoji"] {
            assert!(selected_path_matches(
                &mut fonts,
                assets.set.as_ref().unwrap(),
                role,
                assets.family(role).unwrap()
            ));
        }
        assert!(
            fonts
                .collection
                .family_id(assets.family("sans").unwrap())
                .is_some()
        );
    }
}
