//! Theme (ced E1 plan D17): the effective ctk theme selection — shared
//! `cosmix_config::store::config_dir()/theme.conf.mix` layered with the
//! per-app `<AppDirs ced>/config/theme.conf.mix` (`ctk/src/theme.rs:3121-3137`
//! rule) — compiled with cosmix-design in the user's mode, re-resolved on the
//! `theme.changed` topic. Produces the chrome `Tokens`, the editor
//! [`Palette`](crate::editor::Palette) (every `HlClass` ≥ 3:1 against the
//! background, else pulled toward the text colour) and fonts from the `Mono` /
//! `Ui` typography roles.
//!
//! Desktop-wide theming is mandatory: there is no colour literal here. Every
//! colour is a compiled design token or a mix of two of them. A design that
//! fails to compile falls back to the shared preview palette of
//! `cosmix-iced-widgets` and says so, once, in [`Theme::notes`].
//!
//! Highlight classes map onto the tokens the design already has (`syntax.*`
//! tokens are out of E1, plan §10): keywords take the accent, strings the
//! success status, numbers and constants the warning status, and so on.

use std::path::{Path, PathBuf};

use cosmix_design::{
    DesignCompileResult, DesignContext, LinearRgba, Mode, ResolvedDictionary, ResolvedTypeRecord, Scheme,
    SourceIdentity, TypographyRole,
};
use cosmix_edit_client::highlight::HlClass;
use cosmix_iced_widgets::Tokens;
use cosmix_iced_widgets::tokens::colour;
use iced::Color;

use crate::editor::{HL_CLASSES, Palette};

/// The app identity the design compiler selects a per-app overlay by.
const APP: &str = "ced";

/// Minimum contrast of every highlight colour against the editor background.
pub const MIN_HL_CONTRAST: f64 = 3.0;

pub struct Theme {
    pub palette: Palette,
    /// Chrome colours (menu, tabs, status, dialogs).
    pub tokens: cosmix_iced_widgets::tokens::Tokens,
    /// Mono role: family name and size in px.
    pub mono: (String, f32),
    /// Ui role: family name and size in px.
    pub ui: (String, f32),
    /// The resolved selection (for `ced.info` and the About box).
    pub scheme: Scheme,
    pub mode: Mode,
    /// The fonts to hand iced, resolved to installed families.
    pub mono_font: iced::Font,
    pub ui_font: iced::Font,
    /// Chrome colours the `Tokens` set does not carry.
    pub chrome: Chrome,
    /// Something went wrong resolving (shown once in the status bar).
    pub notes: Option<String>,
}

/// Extra chrome colours, all tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Chrome {
    /// `secondary` surface: tab strip, status bar, inactive tabs.
    pub secondary: Color,
    pub secondary_text: Color,
    /// `palette.accent.default`: agent markers, the active-tab rule.
    pub accent: Color,
    /// `status.warning`: the warning infobar edge.
    pub warning: Color,
    /// `status.success`.
    pub success: Color,
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
/// the editor.
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

pub fn resolve(app_override: Option<&std::path::Path>) -> Theme {
    let mut notes = Vec::new();
    let shared = shared_theme_path();
    let selection = read_selection(Some(&shared), app_override, &mut notes);
    resolve_selection(&selection, notes)
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
    let (tokens, chrome, palette, typography) = match compiled {
        Ok(Compiled { dictionary, typography }) => {
            match (Tokens::from_dictionary(&dictionary), build_palette(&dictionary)) {
                (Ok(tokens), Ok((palette, chrome))) => (tokens, chrome, palette, Some(typography)),
                (Err(error), _) => {
                    notes.push(format!("design dictionary: {error}"));
                    fallback()
                }
                (_, Err(missing)) => {
                    notes.push(format!("design dictionary lacks {missing}"));
                    fallback()
                }
            }
        }
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
        palette,
        tokens,
        mono: (family_name(&mono_font), mono.font_size as f32),
        ui: (family_name(&ui_font), ui.font_size as f32),
        scheme: selection.scheme,
        mode: selection.mode,
        mono_font,
        ui_font,
        chrome,
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
fn fallback() -> (Tokens, Chrome, Palette, Option<cosmix_design::ResolvedTypography>) {
    let t = Tokens::default();
    let chrome = Chrome {
        secondary: t.muted_surface,
        secondary_text: t.text,
        accent: t.ring,
        warning: t.primary,
        success: t.primary,
    };
    let hl = [t.text; HL_CLASSES];
    let palette = Palette {
        background: t.surface,
        text: t.text,
        gutter_background: t.surface,
        gutter_text: t.muted_text,
        current_line: t.muted_surface,
        selection: t.selection,
        caret: t.ring,
        human_other: t.primary,
        agent: t.ring,
        error: t.destructive,
        warning: t.primary,
        note: t.muted_text,
        highlight: hl,
    };
    (t, chrome, palette, None)
}

/// Build the editor palette and the extra chrome colours; `Err(name)` names
/// the first missing token.
pub fn build_palette(d: &ResolvedDictionary) -> Result<(Palette, Chrome), String> {
    let prim = |name: &str| d.colours.primitives.get(name).copied().ok_or_else(|| name.to_owned());
    let pair = |name: &str| d.colours.pairs.get(name).ok_or_else(|| format!("pair {name}"));
    let non_text = |name: &str| d.colours.non_text.get(name).map(|v| v.value).ok_or_else(|| name.to_owned());

    let base = pair("base")?;
    let secondary = pair("secondary")?;
    let muted = pair("muted")?;
    let accent_pair = pair("accent")?;
    let primary = pair("primary")?;
    let destructive = pair("destructive")?;
    let background = base.rendered_surface;
    let text = base.rendered_foreground;
    let accent = prim("palette.accent.default")?;
    let success = prim("status.success")?;
    let warning = prim("status.warning")?;
    let muted_fg = muted.rendered_foreground;

    let mut highlight = [colour(text); HL_CLASSES];
    for class in ALL_CLASSES {
        highlight[class as usize] = colour(legible(class_token(d, class)?, text, background));
    }
    let palette = Palette {
        background: colour(background),
        text: colour(text),
        gutter_background: colour(background),
        gutter_text: colour(muted_fg),
        current_line: colour(secondary.rendered_surface),
        selection: colour(accent_pair.rendered_surface),
        caret: colour(non_text("ring")?),
        human_other: colour(primary.rendered_surface),
        agent: colour(accent),
        error: colour(destructive.rendered_surface),
        warning: colour(warning),
        note: colour(muted_fg),
        highlight,
    };
    let chrome = Chrome {
        secondary: colour(secondary.rendered_surface),
        secondary_text: colour(secondary.rendered_foreground),
        accent: colour(accent),
        warning: colour(warning),
        success: colour(success),
    };
    Ok((palette, chrome))
}

/// The token a highlight class is drawn with, before the legibility pull.
pub fn class_token(d: &ResolvedDictionary, class: HlClass) -> Result<LinearRgba, String> {
    let prim = |name: &str| d.colours.primitives.get(name).copied().ok_or_else(|| name.to_owned());
    let base = d.colours.pairs.get("base").ok_or("pair base")?;
    let muted = d.colours.pairs.get("muted").ok_or("pair muted")?;
    Ok(match class {
        HlClass::Plain | HlClass::Variable | HlClass::Operator => base.rendered_foreground,
        HlClass::Comment | HlClass::Punctuation | HlClass::Meta => muted.rendered_foreground,
        HlClass::Keyword | HlClass::Heading => prim("palette.accent.default")?,
        HlClass::Type | HlClass::Function | HlClass::Link => prim("palette.accent.hover")?,
        HlClass::String | HlClass::Inserted => prim("status.success")?,
        HlClass::Number | HlClass::Constant => prim("status.warning")?,
        HlClass::Deleted | HlClass::Invalid => prim("status.danger")?,
    })
}

/// Every [`HlClass`], in discriminant order.
pub const ALL_CLASSES: [HlClass; HL_CLASSES] = [
    HlClass::Plain,
    HlClass::Comment,
    HlClass::Keyword,
    HlClass::String,
    HlClass::Number,
    HlClass::Constant,
    HlClass::Type,
    HlClass::Function,
    HlClass::Variable,
    HlClass::Operator,
    HlClass::Punctuation,
    HlClass::Meta,
    HlClass::Inserted,
    HlClass::Deleted,
    HlClass::Heading,
    HlClass::Link,
    HlClass::Invalid,
];

/// `candidate` if it reaches [`MIN_HL_CONTRAST`] on `background`; otherwise
/// the nearest mix toward `text` that does (keeping as much of the hue as the
/// contrast allows), and `text` itself as the last resort.
pub fn legible(candidate: LinearRgba, text: LinearRgba, background: LinearRgba) -> LinearRgba {
    if cosmix_design::contrast_ratio(candidate, background) >= MIN_HL_CONTRAST {
        return candidate;
    }
    for step in 1..10 {
        let t = f64::from(step) / 10.0;
        let mixed = mix(candidate, text, t);
        if cosmix_design::contrast_ratio(mixed, background) >= MIN_HL_CONTRAST {
            return mixed;
        }
    }
    text
}

fn mix(a: LinearRgba, b: LinearRgba, t: f64) -> LinearRgba {
    LinearRgba {
        red: a.red + (b.red - a.red) * t,
        green: a.green + (b.green - a.green) * t,
        blue: a.blue + (b.blue - a.blue) * t,
        alpha: a.alpha + (b.alpha - a.alpha) * t,
    }
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
    /// An iced theme for the stock widgets (text inputs, scrollables,
    /// buttons), built from the same tokens.
    pub fn iced_theme(&self) -> iced::Theme {
        iced::Theme::custom(
            "cosmix-ced",
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

    /// The Mono role at `px` (zoom), as `(font, size)`.
    pub fn mono_at(&self, px: Option<u16>) -> (iced::Font, f32) {
        (self.mono_font, px.map_or(self.mono.1, f32::from))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compiled(scheme: Scheme, mode: Mode) -> ResolvedDictionary {
        compile(&Selection { scheme, mode, design_source: None })
            .unwrap_or_else(|e| panic!("{}/{}: {e}", scheme.name(), mode.name()))
            .dictionary
    }

    fn linear(c: Color) -> LinearRgba {
        let lin = |c: f32| {
            let c = f64::from(c);
            if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
        };
        LinearRgba { red: lin(c.r), green: lin(c.g), blue: lin(c.b), alpha: 1.0 }
    }

    /// Plan §7.1: every `HlClass` ≥ 3:1 against the editor background in
    /// every scheme × mode of the embedded source (measured on the 8-bit
    /// colour actually drawn), with at most 2 classes per scheme needing the
    /// pull toward the text colour.
    #[test]
    fn every_highlight_class_is_legible_in_every_scheme_and_mode() {
        for scheme in Scheme::ALL {
            for mode in Mode::ALL {
                let d = compiled(scheme, mode);
                let (palette, _) = build_palette(&d).expect("tokens present");
                let bg = d.colours.pairs["base"].rendered_surface;
                let mut pulled = Vec::new();
                for class in ALL_CLASSES {
                    let ratio = cosmix_design::contrast_ratio(linear(palette.hl(class)), linear(palette.background));
                    assert!(ratio >= MIN_HL_CONTRAST - 0.05, "{}/{} {class:?}: {ratio:.2}:1", scheme.name(), mode.name());
                    if cosmix_design::contrast_ratio(class_token(&d, class).unwrap(), bg) < MIN_HL_CONTRAST {
                        pulled.push(class);
                    }
                }
                assert!(pulled.len() <= 2, "{}/{}: {pulled:?} needed the pull", scheme.name(), mode.name());
            }
        }
    }

    #[test]
    fn keyword_and_comment_differ_so_highlighting_is_visible() {
        for scheme in Scheme::ALL {
            for mode in Mode::ALL {
                let (palette, _) = build_palette(&compiled(scheme, mode)).unwrap();
                assert_ne!(palette.hl(HlClass::Keyword), palette.hl(HlClass::Comment));
                assert_ne!(palette.background, palette.text);
                assert_ne!(palette.selection, palette.background, "{}/{}", scheme.name(), mode.name());
            }
        }
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
    fn the_embedded_design_resolves_for_ced() {
        let theme = resolve_selection(&Selection { scheme: Scheme::Ocean, mode: Mode::Dark, design_source: None }, Vec::new());
        assert!(theme.notes.is_none(), "{:?}", theme.notes);
        assert_ne!(theme.tokens.surface, Tokens::default().surface, "not the fallback palette");
        assert!((theme.mono.1 - 16.0).abs() < 0.01, "Mono role is 16 px: {}", theme.mono.1);
    }
}
