use bevy::prelude::Resource;
use cosmix_deco::{ChromeStyle, DecoTheme, Mode, Scheme, presets};

#[derive(Clone, Debug, PartialEq, Resource)]
pub(crate) struct DecorationStartup {
    pub(crate) enabled: bool,
    pub(crate) theme: DecoTheme,
    pub(crate) title_typography: cosmix_design::ResolvedTypeRecord,
}

impl DecorationStartup {
    pub(crate) fn resolve(enabled: bool, style: ChromeStyle) -> Self {
        // Chrome has no live design loader; read the shared design at startup.
        // Headless tests inject their source rather than inheriting host config.
        #[cfg(all(feature = "bus", not(test)))]
        let source =
            std::fs::read_to_string(cosmix_config::store::config_dir().join("theme.conf.mix")).ok();
        #[cfg(any(not(feature = "bus"), test))]
        let source: Option<String> = None;
        Self::resolve_with_source(enabled, style, source.as_deref())
    }

    fn resolve_with_source(enabled: bool, style: ChromeStyle, source: Option<&str>) -> Self {
        let compiled = source.and_then(|source| {
            let document = cosmix_design::parse_design_source(
                cosmix_design::SourceIdentity::new("chrome:shared-design"),
                source,
            )
            .ok()?;
            match cosmix_design::compile_design(&document, cosmix_design::DesignContext::default())
            {
                cosmix_design::DesignCompileResult::Success(success) => Some(success.candidate),
                _ => None,
            }
        });
        let title = cosmix_design::active_typography(
            compiled.as_ref().map(|design| design.typography()),
            cosmix_design::TypographyRole::UiDisplay,
        );
        let mut theme = presets::resolve(style, Scheme::Ocean, Mode::Light);
        theme.metrics.title_font_family = cosmix_deco::DecoFontFamily::Named(title.family.clone());
        theme.metrics.title_size_px = title.font_size as f32;
        theme.metrics.title_font_weight = cosmix_deco::DecoFontWeight(title.weight);
        Self {
            enabled,
            theme,
            title_typography: title.clone(),
        }
    }
}

impl Default for DecorationStartup {
    fn default() -> Self {
        Self::resolve(true, ChromeStyle::Mac)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_decoration_startup_is_enabled_mac_ocean_light() {
        let startup = DecorationStartup::default();

        assert!(startup.enabled);
        assert_eq!(startup.theme.style, ChromeStyle::Mac);
        assert_eq!(startup.theme.scheme, Scheme::Ocean);
        assert_eq!(startup.theme.mode, Mode::Light);
    }

    #[test]
    fn compiled_display_role_overrides_chrome_metrics_and_fallbacks() {
        let source = cosmix_design::EMBEDDED_DEFAULT_SOURCE.replace(
            "ui_display: { family: \"SF Pro Display\", fallbacks: [\"Inter\", \"Noto Sans\", \"DejaVu Sans\"], generic: \"sans_serif\", logical_px: 14.666666666666666, weight: 300 }",
            "ui_display: { family: \"Example Display\", fallbacks: [\"Example Sans\"], generic: \"sans_serif\", logical_px: 18, weight: 500 }",
        );
        assert_ne!(source, cosmix_design::EMBEDDED_DEFAULT_SOURCE);
        let startup = DecorationStartup::resolve_with_source(true, ChromeStyle::Mac, Some(&source));
        assert_eq!(
            startup.theme.metrics.title_font_family,
            cosmix_deco::DecoFontFamily::Named("Example Display".into())
        );
        assert_eq!(startup.theme.metrics.title_size_px, 18.0);
        assert_eq!(
            startup.theme.metrics.title_font_weight,
            cosmix_deco::DecoFontWeight(500)
        );
        assert_eq!(startup.title_typography.fallbacks, ["Example Sans"]);
    }
}
