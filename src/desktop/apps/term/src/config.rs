use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Cursor {
    Block,
    #[default]
    Underline,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub font_px: f32,
    pub scrollback: usize,
    pub cursor: Cursor,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            font_px: 13.0,
            // Preserve the history limit previously passed to Crosswords::new.
            scrollback: 1000,
            cursor: Cursor::Underline,
        }
    }
}

#[derive(bevy::prelude::Resource, Clone, Copy, Debug, Serialize)]
pub struct Settings {
    #[serde(flatten)]
    pub config: Config,
    #[serde(rename = "TERM")]
    pub term: &'static str,
}

pub fn valid_font(px: f32) -> bool {
    (6.0..=48.0).contains(&px)
}

fn parse(source: &str) -> Result<Config, String> {
    let config: Config = cosmix_config::from_conf_mix_str(source).map_err(|e| e.to_string())?;
    if !valid_font(config.font_px) {
        return Err("font_px must be a finite number in 6..48".into());
    }
    if config.scrollback > 1_000_000 {
        return Err("scrollback must be an integer in 0..1000000".into());
    }
    Ok(config)
}

// VERIFY: config-load — explicit XDG user path, independent of checkout discovery.
pub fn config_path(xdg: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    xdg.filter(|path| path.is_absolute())
        .or_else(|| {
            home.filter(|path| path.is_absolute())
                .map(|path| path.join(".config"))
        })
        .map(|path| path.join("cosmix/term.conf.mix"))
}

pub fn load(path: Option<&Path>) -> Config {
    let result = path
        .ok_or_else(|| "no XDG_CONFIG_HOME or HOME config directory".to_owned())
        .and_then(|path| {
            std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
        })
        .and_then(|source| parse(&source));
    // VERIFY: malformed-defaults — reject the whole file, log once at startup.
    result.unwrap_or_else(|error| {
        eprintln!("term config: {error}; using defaults");
        Config::default()
    })
}

// VERIFY: term-selection — query the actual database without invoking a shell.
// infocmp handles TERMINFO, TERMINFO_DIRS, ~/.terminfo and system databases.
// Missing tools, missing entries and failed probes all keep the portable TERM.
pub fn selected_term() -> &'static str {
    term_for_probe(
        Command::new("infocmp")
            .args(["-x", "xterm-rio"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success()),
    )
}

fn term_for_probe(available: bool) -> &'static str {
    if available {
        "xterm-rio"
    } else {
        "xterm-256color"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_keys_and_example() {
        assert_eq!(parse("{}").unwrap(), Config::default());
        assert_eq!(
            parse(include_str!("../term.example.conf.mix")).unwrap(),
            Config::default()
        );
        assert_eq!(parse("font_px: 18.5").unwrap().font_px, 18.5);
        for source in [
            "font_px: 6",
            "font_px: 48",
            "scrollback: 0",
            "scrollback: 1000000",
            "cursor: \"block\"",
        ] {
            assert!(parse(source).is_ok(), "{source}");
        }
    }

    #[test]
    fn rejects_invalid_values_and_executable_mix() {
        for source in [
            "font_px: 5.9",
            "font_px: 48.1",
            "font_px: \"18\"",
            "font_px: true",
            "font_px: nil",
            "scrollback: -1",
            "scrollback: 1000001",
            "scrollback: 1.5",
            "cursor: \"beam\"",
            "font_pxx: 18",
            "font_px: read_file(\"secret\")",
        ] {
            assert!(parse(source).is_err(), "accepted {source}");
        }
        assert!(!valid_font(f32::NAN));
        assert!(!valid_font(f32::INFINITY));
    }

    #[test]
    fn malformed_and_missing_file_use_defaults() {
        let dir = std::env::temp_dir().join(format!("term-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("term.conf.mix");
        for source in ["font_px: [", "font_px: 18\nscrollback: -1"] {
            std::fs::write(&path, source).unwrap();
            assert_eq!(load(Some(&path)), Config::default());
        }
        std::fs::write(&path, "font_px: 20\ncursor: \"block\"").unwrap();
        assert_eq!(load(Some(&path)).font_px, 20.0);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(load(Some(&path)), Config::default());
        assert_eq!(load(None), Config::default());
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn xdg_path_and_home_fallback() {
        let home = Some(PathBuf::from("/home/example"));
        assert_eq!(
            config_path(Some("/tmp/config".into()), home.clone()),
            Some("/tmp/config/cosmix/term.conf.mix".into())
        );
        for xdg in [None, Some("".into()), Some("relative".into())] {
            assert_eq!(
                config_path(xdg, home.clone()),
                Some("/home/example/.config/cosmix/term.conf.mix".into())
            );
        }
        assert_eq!(config_path(None, None), None);
    }

    #[test]
    fn term_selection_success_and_failure() {
        assert_eq!(term_for_probe(true), "xterm-rio");
        assert_eq!(term_for_probe(false), "xterm-256color");
    }
}
