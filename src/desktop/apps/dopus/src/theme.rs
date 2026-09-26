//! Theme: the effective ctk theme selection — shared
//! `cosmix_config::store::config_dir()/theme.conf.mix` layered with the
//! per-app `<AppDirs dopus>/config/theme.conf.mix` (`ctk/src/theme.rs`
//! rule) — compiled with cosmix-design in the user's mode, re-resolved on
//! the `theme.changed` topic and by the `theme.*` actions.
//!
//! Desktop-wide theming is mandatory: there is no colour literal anywhere in
//! dopus. Every colour is a compiled design token or a mix of two of them. A
//! design that fails to compile falls back to the shared preview palette of
//! `cosmix-iced-widgets` and says so, once, in [`Theme::notes`].
//!
//! Unlike ced there is no editor palette: rows, headers and the status bar
//! draw from the chrome [`Tokens`] plus a few extra token colours in
//! [`Chrome`].

use std::path::{Path, PathBuf};

use cosmix_design::{
    DesignCompileResult, DesignContext, Mode, ResolvedDictionary, ResolvedTypeRecord, Scheme,
    SourceIdentity, TypographyRole,
};
use cosmix_iced_widgets::Tokens;
use cosmix_iced_widgets::tokens::colour;
use iced::Color;

/// The app identity the design compiler selects a per-app overlay by.
const APP: &str = "dopus";

pub struct Theme {
    /// Chrome colours (rows, headers, status bar, stock widgets).
    pub tokens: Tokens,
    /// Extra token colours the `Tokens` set does not carry.
    pub chrome: Chrome,
    /// Mono role: family name and size in px (row secondary columns).
    pub mono: (String, f32),
    /// Ui role: family name and size in px.
    pub ui: (String, f32),
    /// The fonts to hand iced, resolved to installed families.
    pub mono_font: iced::Font,
    pub ui_font: iced::Font,
    /// The resolved selection (for `dopus.state` and theme actions).
    pub scheme: Scheme,
    pub mode: Mode,
    /// Something went wrong resolving (shown once in the status bar).
    pub notes: Option<String>,
}

/// Extra chrome colours, all tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Chrome {
    /// `secondary` surface: the header strip, status bar, inactive pane.
    pub secondary: Color,
    pub secondary_text: Color,
    /// `palette.accent.default`: the active-pane border.
    pub accent: Color,
    /// `status.success`.
    pub success: Color,
    /// `status.warning`.
    pub warning: Color,
}

/// The effective `(scheme, mode)` and the design source to compile.
#[derive(Debug, Clone, PartialEq)]
pub struct Selection {
    pub scheme: Scheme,
    pub mode: Mode,
    /// A user design (`design:` in the shared theme file) replaces the
    /// embedded one; `None` = embedded.
    pub design_source: Option<(PathBuf, String)>,
}

#[derive(serde::Deserialize, Default)]
struct ThemeFileSelection {
    scheme: Option<String>,
    mode: Option<String>,
    design: Option<serde::de::IgnoredAny>,
}

/// The shared theme path (`ctk::theme::shared_theme_path`).
pub fn shared_theme_path() -> PathBuf {
    cosmix_config::store::config_dir().join("theme.conf.mix")
}

/// Read `shared ← app` selections. Missing files are skipped; malformed ones
/// are skipped with a note, exactly as ctk does — a broken theme never bricks
/// the app.
pub fn read_selection(shared: Option<&Path>, app: Option<&Path>, notes: &mut Vec<String>) -> Selection {
    let mut selection = Selection { scheme: Scheme::default(), mode: Mode::default(), design_source: None };
    for (layer, path) in [("shared", shared), ("app", app)] {
        let Some(path) = path.filter(|p| p.exists()) else { continue };
        match cosmix_config::store::load_conf_mix_path::<ThemeFileSelection>(path) {
            Ok(file) => {
                if let Some(name) = file.scheme {
                    match Scheme::from_name(&name) {
                        Some(scheme) => selection.scheme = scheme,
                        None => notes.push(format!("{layer} theme: unknown scheme {name:?}")),
                    }
                }
                if let Some(name) = file.mode {
                    match Mode::from_name(&name) {
                        Some(mode) => selection.mode = mode,
                        None => notes.push(format!("{layer} theme: unknown mode {name:?}")),
                    }
                }
                if layer == "shared"
                    && file.design.is_some()
                    && let Ok(text) = std::fs::read_to_string(path)
                {
                    selection.design_source = Some((path.to_path_buf(), text));
                }
            }
            Err(error) => notes.push(format!("{layer} theme skipped: {error:#}")),
        }
    }
    selection
}

/// Resolve from the theme files, with an in-session `(scheme, mode)` override
/// (the `theme.*` actions and `dopus.theme.set`; P1 does not persist it —
/// filemgr's shared-file write arrives with P2's config work).
pub fn resolve_selected(override_selection: Option<(Scheme, Mode)>, app_override: Option<&Path>) -> Theme {
    let mut notes = Vec::new();
    let shared = shared_theme_path();
    let mut selection = read_selection(Some(&shared), app_override, &mut notes);
    if let Some((scheme, mode)) = override_selection {
        selection.scheme = scheme;
        selection.mode = mode;
    }
    resolve_selection(&selection, notes)
}

pub fn resolve(app_override: Option<&Path>) -> Theme {
    resolve_selected(None, app_override)
}

/// Compile `selection` into a [`Theme`].
pub fn resolve_selection(selection: &Selection, mut notes: Vec<String>) -> Theme {
    let compiled = compile(selection).or_else(|error| {
        if selection.design_source.is_some() {
            notes.push(format!("{error}; using the embedded design"));
            compile(&Selection { design_source: None, ..selection.clone() })
        } else {
            Err(error)
        }
    });
    let (tokens, chrome, typography) = match compiled {
        Ok(Compiled { dictionary, typography }) => match (Tokens::from_dictionary(&dictionary), build_chrome(&dictionary)) {
            (Ok(tokens), Ok(chrome)) => (tokens, chrome, Some(typography)),
            (Err(error), _) | (_, Err(error)) => {
                notes.push(format!("design dictionary: {error}"));
                fallback()
            }
        },
        Err(error) => {
            notes.push(error);
            fallback()
        }
    };
    let role = |r: TypographyRole| -> ResolvedTypeRecord {
        typography
            .as_ref()
            .and_then(|t: &cosmix_design::ResolvedTypography| t.role(r).cloned())
            .unwrap_or_else(|| cosmix_design::default_typography(r).clone())
    };
    let mono = role(TypographyRole::Mono);
    let ui = role(TypographyRole::Ui);
    let mono_font = font_for(&mono, true);
    let ui_font = font_for(&ui, false);
    Theme {
        tokens,
        chrome,
        mono: (family_name(&mono_font), mono.font_size as f32),
        ui: (family_name(&ui_font), ui.font_size as f32),
        scheme: selection.scheme,
        mode: selection.mode,
        mono_font,
        ui_font,
        notes: (!notes.is_empty()).then(|| notes.join("; ")),
    }
}

struct Compiled {
    dictionary: ResolvedDictionary,
    typography: cosmix_design::ResolvedTypography,
}

fn compile(selection: &Selection) -> Result<Compiled, String> {
    let (identity, source) = match &selection.design_source {
        Some((path, text)) => (format!("file:{}", path.display()), text.as_str()),
        None => ("embedded:cosmix-design-default".to_owned(), cosmix_design::EMBEDDED_DEFAULT_SOURCE),
    };
    let document = cosmix_design::parse_design_source(SourceIdentity::new(identity.clone()), source)
        .map_err(|error| format!("design source {identity}: {error}"))?;
    let context = DesignContext {
        scheme: selection.scheme,
        mode: selection.mode,
        app: Some(APP.to_owned()),
        ..DesignContext::default()
    };
    match cosmix_design::compile_design(&document, context) {
        DesignCompileResult::Success(success) => Ok(Compiled {
            dictionary: success.candidate.dictionary().clone(),
            typography: success.candidate.typography().clone(),
        }),
        DesignCompileResult::Fatal(_) => Err(format!("design {identity} does not compile")),
    }
}

/// The shared preview palette, used only when the design cannot be compiled.
fn fallback() -> (Tokens, Chrome, Option<cosmix_design::ResolvedTypography>) {
    let t = Tokens::default();
    let chrome = Chrome {
        secondary: t.muted_surface,
        secondary_text: t.text,
        accent: t.ring,
        success: t.primary,
        warning: t.primary,
    };
    (t, chrome, None)
}

/// The extra chrome colours; `Err(name)` names the first missing token.
pub fn build_chrome(d: &ResolvedDictionary) -> Result<Chrome, String> {
    let prim = |name: &str| d.colours.primitives.get(name).copied().ok_or_else(|| name.to_owned());
    let pair = |name: &str| d.colours.pairs.get(name).ok_or_else(|| format!("pair {name}"));
    let secondary = pair("secondary")?;
    Ok(Chrome {
        secondary: colour(secondary.rendered_surface),
        secondary_text: colour(secondary.rendered_foreground),
        accent: colour(prim("palette.accent.default")?),
        success: colour(prim("status.success")?),
        warning: colour(prim("status.warning")?),
    })
}

/// The first installed family of the role (named family, then its
/// fallbacks), else the generic family. The name is interned once per
/// process: iced fonts name families with `&'static str`.
fn font_for(record: &ResolvedTypeRecord, monospace: bool) -> iced::Font {
    use iced::advanced::graphics::text::font_system;
    let names: Vec<String> = std::iter::once(record.family.clone()).chain(record.fallbacks.iter().cloned()).collect();
    let (installed, has_light) = {
        let mut system = font_system().write().expect("font system");
        let db = system.raw().db();
        let found = names.iter().find(|name| {
            db.faces().any(|face| face.families.iter().any(|(family, _)| family.eq_ignore_ascii_case(name)))
        });
        let light = found.is_some_and(|name| {
            db.faces().any(|face| {
                face.weight.0 == 300 && face.families.iter().any(|(family, _)| family.eq_ignore_ascii_case(name))
            })
        });
        (found.cloned(), light)
    };
    let family = match installed {
        Some(name) => iced::font::Family::Name(intern(&name)),
        None if monospace => iced::font::Family::Monospace,
        None => iced::font::Family::SansSerif,
    };
    let weight = match cosmix_design::family_font_weight(record.weight, has_light) {
        0..=150 => iced::font::Weight::Thin,
        151..=250 => iced::font::Weight::ExtraLight,
        251..=350 => iced::font::Weight::Light,
        351..=450 => iced::font::Weight::Normal,
        451..=550 => iced::font::Weight::Medium,
        551..=650 => iced::font::Weight::Semibold,
        651..=750 => iced::font::Weight::Bold,
        751..=850 => iced::font::Weight::ExtraBold,
        _ => iced::font::Weight::Black,
    };
    iced::Font { family, weight, ..iced::Font::DEFAULT }
}

fn family_name(font: &iced::Font) -> String {
    match font.family {
        iced::font::Family::Name(name) => name.to_owned(),
        iced::font::Family::Monospace => "monospace".to_owned(),
        _ => "sans-serif".to_owned(),
    }
}

fn intern(name: &str) -> &'static str {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static NAMES: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let mut names = NAMES.get_or_init(Default::default).lock().expect("font names");
    if let Some(existing) = names.get(name) {
        return existing;
    }
    let leaked: &'static str = Box::leak(name.to_owned().into_boxed_str());
    names.insert(leaked);
    leaked
}

impl Theme {
    /// An iced theme for the stock widgets (buttons, scrollables, containers),
    /// built from the same tokens.
    pub fn iced_theme(&self) -> iced::Theme {
        iced::Theme::custom(
            "cosmix-dopus",
            iced::theme::Palette {
                background: self.tokens.surface,
                text: self.tokens.text,
                primary: self.tokens.primary,
                success: self.chrome.success,
                warning: self.chrome.warning,
                danger: self.tokens.destructive,
            },
        )
    }

    /// The Ui role at its resolved size.
    pub fn ui_px(&self) -> f32 {
        self.ui.1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_design_resolves_for_dopus() {
        let theme = resolve_selection(&Selection { scheme: Scheme::Ocean, mode: Mode::Dark, design_source: None }, Vec::new());
        assert!(theme.notes.is_none(), "{:?}", theme.notes);
        assert_ne!(theme.tokens.surface, Tokens::default().surface, "not the fallback palette");
        assert!((theme.mono.1 - 16.0).abs() < 0.01, "Mono role is 16 px: {}", theme.mono.1);
    }

    #[test]
    fn selection_layers_shared_then_app_and_skips_bad_files() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared.conf.mix");
        let app = dir.path().join("app.conf.mix");
        std::fs::write(&shared, "scheme: \"forest\"\nmode: \"dark\"\n").unwrap();
        std::fs::write(&app, "mode: \"light\"\n").unwrap();
        let mut notes = Vec::new();
        let s = read_selection(Some(&shared), Some(&app), &mut notes);
        assert_eq!((s.scheme, s.mode), (Scheme::Forest, Mode::Light));
        assert!(notes.is_empty(), "{notes:?}");
        std::fs::write(&app, "scheme: \"plaid\"\n").unwrap();
        let s = read_selection(Some(&shared), Some(&app), &mut notes);
        assert_eq!((s.scheme, s.mode), (Scheme::Forest, Mode::Dark));
        assert_eq!(notes.len(), 1, "{notes:?}");
        let s = read_selection(Some(&dir.path().join("missing")), None, &mut Vec::new());
        assert_eq!((s.scheme, s.mode), (Scheme::default(), Mode::default()));
    }

    #[test]
    fn override_selection_wins_over_files() {
        let theme = resolve_selected(Some((Scheme::Mono, Mode::Light)), None);
        assert_eq!((theme.scheme, theme.mode), (Scheme::Mono, Mode::Light));
    }
}
