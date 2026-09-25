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
/// message for the status bar).
pub fn load(path: &std::path::Path) -> (Config, Option<String>) {
    let _ = path;
    todo!("ced E1f")
}
