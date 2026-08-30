use bevy::prelude::Resource;
use cosmix_deco::{ChromeStyle, DecoTheme, Mode, Scheme, presets};

#[derive(Clone, Debug, PartialEq, Resource)]
pub(crate) struct DecorationStartup {
    pub(crate) enabled: bool,
    pub(crate) theme: DecoTheme,
}

impl DecorationStartup {
    pub(crate) fn resolve(enabled: bool, style: ChromeStyle) -> Self {
        Self {
            enabled,
            theme: presets::resolve(style, Scheme::Ocean, Mode::Light),
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
}
