//! Read the host desktop environment's configured UI font, so Quoin's chrome
//! matches the surrounding desktop rather than shipping a fixed face.
//!
//! This is a best-effort *import*, never a dependency: any failure — no config
//! file, an unreadable one, a line that does not parse — yields `None` and the
//! caller keeps CTK's built-in typography. The shell must look reasonable on a
//! machine with no KDE config at all.
//!
//! Only KDE Plasma's `kdeglobals` is read today (the host this port targets).
//! The format is INI; the `[General]` section's `font` key is a Qt font
//! descriptor whose first two comma-separated fields are the family and the
//! point size:
//!
//! ```text
//! [General]
//! font=SF Pro Text,11,-1,5,300,0,0,0,0,0,0,0,0,0,0,1,Light,0,0
//! ```
//!
//! A point size is converted to Bevy's pixel `body_px` at the conventional
//! 96 dpi (`px = pt × 96/72`); per-output scale is applied downstream by the
//! layer host, so this must stay a logical, scale-free value.

use std::path::PathBuf;

/// The desktop's configured UI typography, in the shape CTK consumes.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DesktopFont {
    pub family: String,
    pub body_px: f32,
}

/// Points to logical pixels at 96 dpi. Downstream output scale is separate.
fn points_to_px(points: f32) -> f32 {
    points * 96.0 / 72.0
}

/// Resolve the desktop UI font, or `None` to keep CTK's built-in.
pub(crate) fn detect() -> Option<DesktopFont> {
    let path = kdeglobals_path()?;
    let source = std::fs::read_to_string(path).ok()?;
    parse_kdeglobals_font(&source)
}

fn kdeglobals_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(dir).join("kdeglobals"));
    }
    let home = std::env::var_os("HOME").filter(|value| !value.is_empty())?;
    Some(PathBuf::from(home).join(".config").join("kdeglobals"))
}

/// Extract family + body pixel size from a `kdeglobals` INI body.
///
/// Scans for the `[General]` section and its `font=` key. The value's first
/// field is the family; the second is the point size. A missing section, key,
/// empty family or unparseable size all yield `None` — the caller keeps the
/// built-in rather than rendering with a half-read descriptor.
fn parse_kdeglobals_font(source: &str) -> Option<DesktopFont> {
    let mut in_general = false;
    for line in source.lines() {
        let line = line.trim();
        if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_general = section.eq_ignore_ascii_case("General");
            continue;
        }
        if !in_general {
            continue;
        }
        let Some(value) = line.strip_prefix("font=") else {
            continue;
        };
        let mut fields = value.split(',');
        let family = fields.next()?.trim();
        if family.is_empty() {
            return None;
        }
        let points: f32 = fields.next()?.trim().parse().ok()?;
        if !points.is_finite() || points <= 0.0 {
            return None;
        }
        return Some(DesktopFont {
            family: family.to_owned(),
            body_px: points_to_px(points),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_family_and_point_size_from_general() {
        let source = "\
[ColorScheme]
font=Wrong,99

[General]
fixed=SF Mono,12,-1,5,300,0,0,0,0,0
font=SF Pro Text,11,-1,5,300,0,0,0,0,0,0,0,0,0,0,1,Light,0,0
";
        let font = parse_kdeglobals_font(source).expect("font in [General]");
        assert_eq!(font.family, "SF Pro Text");
        // 11 pt at 96 dpi = 14.666…
        assert!((font.body_px - 14.6667).abs() < 0.01, "{}", font.body_px);
    }

    #[test]
    fn font_outside_general_is_ignored() {
        let source = "[ColorScheme]\nfont=Wrong,99\n";
        assert_eq!(parse_kdeglobals_font(source), None);
    }

    #[test]
    fn missing_or_empty_family_yields_none() {
        assert_eq!(parse_kdeglobals_font("[General]\nfont=,11\n"), None);
        assert_eq!(parse_kdeglobals_font("[General]\nother=x\n"), None);
        assert_eq!(parse_kdeglobals_font(""), None);
    }

    #[test]
    fn unparseable_size_yields_none() {
        assert_eq!(
            parse_kdeglobals_font("[General]\nfont=Noto Sans,big\n"),
            None
        );
        assert_eq!(
            parse_kdeglobals_font("[General]\nfont=Noto Sans,-3\n"),
            None
        );
    }
}
