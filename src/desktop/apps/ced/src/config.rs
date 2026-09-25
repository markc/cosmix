//! `<AppDirs ced>/config/ced.conf.mix` (ced E1 plan §4.7) — keys and defaults
//! frozen in Stage S; parsing and `--print-config` land in Stage E1f.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Tab stop width, 1..=16.
    pub tab_size: u8,
    /// Per editd language id: Tab inserts spaces (true) or `\t` (false).
    pub insert_spaces: BTreeMap<String, bool>,
    /// Overrides the `Mono` typography role's size.
    pub font_px: Option<u16>,
    pub show_whitespace: bool,
    pub line_numbers: bool,
    pub remote_carets: bool,
    pub lint_on_save: bool,
    /// UAX #11 ambiguous-width characters measure 2 cells.
    pub ambiguous_wide: bool,
}

impl Default for Config {
    fn default() -> Self {
        let insert_spaces = [("mix", true), ("scene", true), ("mix-data", true), ("rust", true), ("text", false)]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        Self {
            tab_size: 4,
            insert_spaces,
            font_px: None,
            show_whitespace: false,
            line_numbers: true,
            remote_carets: true,
            lint_on_save: true,
            ambiguous_wide: false,
        }
    }
}

impl Config {
    /// Indent width for a language when inserting spaces (mix-family: 2).
    pub fn indent_width(&self, language: &str) -> u8 {
        match language {
            "mix" | "scene" | "mix-data" => 2,
            _ => self.tab_size,
        }
    }
}

/// Read the config file (missing → defaults; malformed → defaults + an error
/// message for the status bar). Out-of-range values are clamped and named in
/// the message rather than refused: a typo must never cost the editor.
pub fn load(path: &std::path::Path) -> (Config, Option<String>) {
    if !path.exists() {
        return (Config::default(), None);
    }
    match cosmix_config::store::load_conf_mix_path::<Config>(path) {
        Ok(config) => config.validated(),
        Err(error) => (Config::default(), Some(format!("{error:#}; using defaults"))),
    }
}

impl Config {
    /// Clamp values the file may carry out of range; say which.
    fn validated(mut self) -> (Config, Option<String>) {
        let mut notes = Vec::new();
        if !(1..=16).contains(&self.tab_size) {
            notes.push(format!("tab_size {} is outside 1..=16", self.tab_size));
            self.tab_size = self.tab_size.clamp(1, 16);
        }
        if let Some(px) = self.font_px
            && !(FONT_PX_MIN..=FONT_PX_MAX).contains(&px)
        {
            notes.push(format!("font_px {px} is outside {FONT_PX_MIN}..={FONT_PX_MAX}"));
            self.font_px = Some(px.clamp(FONT_PX_MIN, FONT_PX_MAX));
        }
        let note = (!notes.is_empty()).then(|| format!("ced.conf.mix: {}", notes.join("; ")));
        (self, note)
    }

    /// Tab inserts spaces for this editd language (unlisted: spaces, except
    /// plain text and makefiles, where a tab is the point).
    pub fn spaces_for(&self, language: &str) -> bool {
        self.insert_spaces.get(language).copied().unwrap_or(!matches!(language, "text" | "make" | "makefile"))
    }

    /// The resolved configuration as pretty JSON (`ced --print-config`).
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }
}

/// Zoom bounds for the text font, logical px.
pub const FONT_PX_MIN: u16 = 6;
pub const FONT_PX_MAX: u16 = 72;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_defaults_without_a_message() {
        let dir = tempfile::tempdir().unwrap();
        let (config, note) = load(&dir.path().join("ced.conf.mix"));
        assert_eq!(config, Config::default());
        assert!(note.is_none());
    }

    #[test]
    fn partial_file_keeps_defaults_and_out_of_range_is_clamped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ced.conf.mix");
        std::fs::write(&path, "tab_size: 40\nshow_whitespace: true\nfont_px: 2\n").unwrap();
        let (config, note) = load(&path);
        assert_eq!(config.tab_size, 16);
        assert_eq!(config.font_px, Some(FONT_PX_MIN));
        assert!(config.show_whitespace);
        assert!(config.line_numbers, "unset keys keep their defaults");
        let note = note.expect("clamping is reported");
        assert!(note.contains("tab_size 40") && note.contains("font_px 2"), "{note}");
    }

    #[test]
    fn malformed_file_is_defaults_with_a_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ced.conf.mix");
        std::fs::write(&path, "tab_size: \"four\"\n").unwrap();
        let (config, note) = load(&path);
        assert_eq!(config, Config::default());
        assert!(note.unwrap().contains("using defaults"));
    }

    #[test]
    fn indent_rules() {
        let c = Config::default();
        assert!(c.spaces_for("mix") && c.spaces_for("rust") && !c.spaces_for("text"));
        assert_eq!(c.indent_width("scene"), 2);
        assert_eq!(c.indent_width("rust"), 4);
    }
}
