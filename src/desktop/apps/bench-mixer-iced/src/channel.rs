//! Per-track note colours, the same spread CTK's `channel_color` draws.
//!
//! The Bevy arm colours each track by walking the accent's hue with a
//! low-discrepancy fraction, in Oklch. `cosmix-design` keeps its Oklch
//! conversion private and iced has no perceptual colour space, so the same
//! arithmetic lives here, on Björn Ottosson's published Oklab matrices — the
//! ones Bevy's `Oklcha` uses.
//!
//! Only the base accent differs between the arms: CTK reads
//! `palette.accent.default` while `cosmix_iced_widgets::Tokens` maps colour
//! pairs, so this arm takes the nearest thing the token set names, `ring`.
//! The spread around it is identical (see `known-deltas.conf.mix`,
//! `colour-role-mapping`).

use iced::Color;

/// The golden-ratio conjugate CTK walks the hue with.
const GOLDEN: f32 = 0.618_034;

/// sRGB component to linear light.
fn to_linear(c: f32) -> f32 {
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Linear light to an sRGB component.
fn to_encoded(c: f32) -> f32 {
    if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

/// `(lightness, chroma, hue degrees)` of an sRGB colour.
fn to_oklch(colour: Color) -> (f32, f32, f32) {
    let (r, g, b) = (
        to_linear(colour.r),
        to_linear(colour.g),
        to_linear(colour.b),
    );
    let l = (0.412_221_47 * r + 0.536_332_54 * g + 0.051_445_995 * b).cbrt();
    let m = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
    let s = (0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_5 * b).cbrt();
    let lightness = 0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s;
    let a = 1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s;
    let b = 0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s;
    let chroma = (a * a + b * b).sqrt();
    let hue = b.atan2(a).to_degrees().rem_euclid(360.0);
    (lightness, chroma, hue)
}

/// An sRGB colour from `(lightness, chroma, hue degrees)`, clamped to gamut.
fn from_oklch(lightness: f32, chroma: f32, hue: f32) -> Color {
    let (a, b) = {
        let radians = hue.to_radians();
        (chroma * radians.cos(), chroma * radians.sin())
    };
    let l = (lightness + 0.396_337_78 * a + 0.215_803_76 * b).powi(3);
    let m = (lightness - 0.105_561_346 * a - 0.063_854_17 * b).powi(3);
    let s = (lightness - 0.089_484_18 * a - 1.291_485_5 * b).powi(3);
    let channel = |value: f32| to_encoded(value).clamp(0.0, 1.0);
    Color::from_rgb(
        channel(4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_94 * s),
        channel(-1.268_438 * l + 2.609_757_4 * m - 0.341_319_4 * s),
        channel(-0.004_196_086_3 * l - 0.703_418_6 * m + 1.707_614_7 * s),
    )
}

/// `track`'s note colour, derived from `accent` exactly as CTK's
/// `channel_color` derives a channel's: track 0 is the accent itself, and
/// every other track walks the hue by a low-discrepancy fraction and shifts
/// lightness with it, so any number of tracks stays evenly spread.
pub fn track_colour(track: u16, accent: Color) -> Color {
    let (lightness, chroma, hue) = to_oklch(accent);
    if track == 0 {
        return from_oklch(lightness, chroma, hue);
    }
    let g = (f32::from(track) * GOLDEN).fract();
    from_oklch(
        (lightness + (g - 0.5) * 0.30).clamp(0.35, 0.92),
        chroma,
        (hue + (g - 0.5) * 110.0).rem_euclid(360.0),
    )
}

/// One colour per track, for `PianoRoll::track_colours`.
pub fn track_palette(tracks: usize, accent: Color) -> Vec<Color> {
    (0..tracks.max(1))
        .map(|track| track_colour(track as u16, accent))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: Color, b: Color, tolerance: f32) -> bool {
        (a.r - b.r).abs() < tolerance
            && (a.g - b.g).abs() < tolerance
            && (a.b - b.b).abs() < tolerance
    }

    #[test]
    fn oklch_round_trips_through_srgb() {
        for colour in [
            Color::from_rgb(0.0, 0.0, 0.0),
            Color::from_rgb(1.0, 1.0, 1.0),
            Color::from_rgb(0.24, 0.75, 0.89),
            Color::from_rgb(0.9, 0.2, 0.35),
            Color::from_rgb(0.05, 0.11, 0.14),
        ] {
            let (l, c, h) = to_oklch(colour);
            assert!(
                close(from_oklch(l, c, h), colour, 1e-3),
                "{colour:?} -> ({l}, {c}, {h}) -> {:?}",
                from_oklch(l, c, h)
            );
        }
    }

    #[test]
    fn oklch_matches_published_anchors() {
        // Ottosson's reference: sRGB white is L=1, C=0; mid grey keeps C=0.
        let (l, c, _) = to_oklch(Color::WHITE);
        assert!((l - 1.0).abs() < 1e-3 && c < 1e-3, "white L={l} C={c}");
        let (l, c, _) = to_oklch(Color::from_rgb(0.5, 0.5, 0.5));
        assert!(c < 1e-3, "grey must have no chroma, got {c}");
        assert!((0.5..0.7).contains(&l), "mid grey L={l}");
    }

    #[test]
    fn track_zero_is_the_accent_and_the_rest_are_spread() {
        let accent = Color::from_rgb8(0x3d, 0xbf, 0xe2);
        assert!(close(track_colour(0, accent), accent, 2e-3));
        let (_, accent_chroma, accent_hue) = to_oklch(accent);
        let mut hues = Vec::new();
        for track in 1..32u16 {
            let colour = track_colour(track, accent);
            let (lightness, chroma, hue) = to_oklch(colour);
            // Chroma is carried over; only hue and lightness move.
            assert!(
                (chroma - accent_chroma).abs() < 0.02,
                "track {track} chroma {chroma} vs {accent_chroma}"
            );
            assert!((0.34..=0.93).contains(&lightness), "track {track}");
            let shift = (hue - accent_hue).rem_euclid(360.0);
            assert!(shift <= 55.5 || shift >= 304.5, "track {track} hue {hue}");
            hues.push(hue);
        }
        // Low discrepancy: 31 tracks must not collapse onto a few hues.
        hues.sort_by(f32::total_cmp);
        hues.dedup_by(|a, b| (*a - *b).abs() < 0.5);
        assert!(hues.len() > 25, "only {} distinct hues", hues.len());
    }

    #[test]
    fn a_palette_is_never_empty() {
        assert_eq!(track_palette(0, Color::WHITE).len(), 1);
        assert_eq!(track_palette(32, Color::WHITE).len(), 32);
    }
}
