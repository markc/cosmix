//! Theme (ced E1 plan D17): the effective ctk theme selection — shared
//! `cosmix_config::store::config_dir()/theme.conf.mix` layered with the
//! per-app `<AppDirs ced>/config/theme.conf.mix` (`ctk/src/theme.rs:3121-3137`
//! rule) — compiled with cosmix-design in the user's mode, re-resolved on the
//! `theme.changed` topic. Produces the chrome `Tokens`, the editor
//! [`Palette`](crate::editor::Palette) (every `HlClass` ≥ 3:1 against the
//! background, else the text colour) and fonts from the `Mono` / `Ui`
//! typography roles. Stage E1f implements it.

use crate::editor::Palette;

pub struct Theme {
    pub palette: Palette,
    /// Chrome colours (menu, tabs, status, dialogs).
    pub tokens: cosmix_iced_widgets::tokens::Tokens,
    /// Mono role: family name and size in px.
    pub mono: (String, f32),
    /// Ui role: family name and size in px.
    pub ui: (String, f32),
}

pub fn resolve(app_override: Option<&std::path::Path>) -> Theme {
    let _ = app_override;
    todo!("ced E1f")
}
