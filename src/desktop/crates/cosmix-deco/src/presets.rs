//! The three built-in chrome themes.
//!
//! Colour provenance:
//! * `mac` — traffic-light and surface values sampled from macOS (and
//!   cross-checked against the MacTahoe aurorae SVGs used in the KDE port).
//!   Redrawn procedurally here: no upstream assets, no licence entanglement.
//! * `win11` — Windows 11 caption metrics and the signature close-hover red.
//! * `cosmix` — the native look: everything neutral derives from the mode,
//!   everything alive derives from the ctk scheme accent (same Oklch hues as
//!   `ctk::theme`, so chrome and widgets agree without coordination).

use crate::geom::{oklch, Srgba};
use crate::theme::{
    pt_to_px, ButtonCluster, ButtonColors, ButtonShape, ButtonSide, CaptionButton, ChromeStyle,
    DecoColors, DecoFontFamily, DecoFontWeight, DecoMetrics, DecoTheme, GlyphPolicy, Mode, Scheme,
    ShadowSpec, TitleAlign,
};

/// The desktop's UI face, and therefore the title face in every chrome style.
///
/// Chrome typography is a property of the desktop, not of the chrome style: a
/// window that looks Windows-ish should still be lettered like everything else
/// on the machine. All three presets share the family, size and weight below;
/// only geometry and colour differ between them.
///
/// A host without this family is not misconfigured — the compositor degrades to
/// the platform UI family and finally to its own embedded face.
pub const DEFAULT_TITLE_FONT_FAMILY: &str = "SF Pro Text";
/// Point size for [`DEFAULT_TITLE_FONT_FAMILY`] (converted to logical px at
/// 96 dpi; output scale is applied later by the compositor).
pub const DEFAULT_TITLE_SIZE_PT: f32 = 11.0;
/// Weight for [`DEFAULT_TITLE_FONT_FAMILY`] — Light reads as chrome, not as
/// content, at title sizes.
pub const DEFAULT_TITLE_FONT_WEIGHT: DecoFontWeight = DecoFontWeight::LIGHT;

fn default_title_font_family() -> DecoFontFamily {
    DecoFontFamily::Named(DEFAULT_TITLE_FONT_FAMILY.to_owned())
}

// ---------------------------------------------------------------- mac

/// macOS-like chrome: traffic lights top-left, centred title, 12px lights
/// with cluster-hover glyphs, soft high shadow.
pub fn mac(mode: Mode) -> DecoTheme {
    let dark = mode == Mode::Dark;

    let traffic = |fill: u32, glyph: u32| ButtonColors {
        fill_idle: Srgba::hex(fill),
        // Hover keeps the fill; the glyph appearing *is* the hover affordance.
        fill_hover: Srgba::hex(fill),
        fill_pressed: Srgba::hex(fill).with_alpha(0.8),
        fill_unfocused: if dark { Srgba::hex(0x4A4A4C) } else { Srgba::hex(0xDBDBDB) },
        glyph: Srgba::hex(glyph),
        glyph_hover: Srgba::hex(glyph),
    };

    DecoTheme {
        style: ChromeStyle::Mac,
        scheme: Scheme::Mono, // placeholder — resolve() records the caller's scheme
        mode,
        metrics: DecoMetrics {
            titlebar_height: 28.0,
            border_thickness: 0.0,
            resize_band: 8.0,
            corner_radius: 12.0,
            title_size_px: pt_to_px(DEFAULT_TITLE_SIZE_PT),
            title_font_family: default_title_font_family(),
            title_font_weight: DEFAULT_TITLE_FONT_WEIGHT,
            title_align: TitleAlign::Center,
            title_pad: 8.0,
            shadow: ShadowSpec {
                softness: 40.0,
                offset_y: 12.0,
                color: Srgba::hex(0x000000),
                alpha_focused: 0.45,
                alpha_unfocused: 0.22,
            },
        },
        colors: DecoColors {
            titlebar_focused: if dark { Srgba::hex(0x2E2E30) } else { Srgba::hex(0xF0EEEC) },
            titlebar_unfocused: if dark { Srgba::hex(0x252527) } else { Srgba::hex(0xF6F5F4) },
            titlebar_divider: if dark {
                Srgba::hex(0x000000).with_alpha(0.35)
            } else {
                Srgba::hex(0x000000).with_alpha(0.12)
            },
            title_text_focused: if dark { Srgba::hex(0xE8E8E8) } else { Srgba::hex(0x3A3A3C) },
            title_text_unfocused: if dark { Srgba::hex(0x7C7C7E) } else { Srgba::hex(0xAAAAAA) },
            border_focused: Srgba::TRANSPARENT,
            border_unfocused: Srgba::TRANSPARENT,
        },
        buttons: ButtonCluster {
            side: ButtonSide::Left,
            order: [CaptionButton::Close, CaptionButton::Minimize, CaptionButton::Maximize],
            shape: ButtonShape::Circle { diameter: 12.0 },
            gap: 8.0,
            edge_inset: 12.0,
            // The disc is the affordance here; the glyph is a hover detail
            // inside an already-filled 12px light.
            glyph_extent_ratio: 0.36,
            glyphs: GlyphPolicy::ClusterHover,
            close: traffic(0xFF5F57, 0x730B01),
            minimize: traffic(0xFEBC2E, 0x7A5A02),
            maximize: traffic(0x28C840, 0x0A530F),
        },
    }
}

// ---------------------------------------------------------------- win11

/// Windows 11-like chrome: flat full-height caption targets top-right, leading
/// title, the signature red close hover, tight shadow, 8px corners.
///
/// The caption cell is **38px wide, not the Windows 11 reference 46px** — a
/// deliberate divergence (Mark's verdict on the Phase 3 close smoke,
/// 2026-08-11: "too large with too much space between them"). Cell width is
/// the single lever for both complaints: the glyph is sized off `min(w, h)`,
/// which is the 32px height here, so narrowing the cell leaves the glyph
/// untouched and only tightens the slab and the glyph-to-glyph pitch (46 → 38).
pub fn win11(mode: Mode) -> DecoTheme {
    let dark = mode == Mode::Dark;

    let text = if dark { Srgba::hex(0xFFFFFF) } else { Srgba::hex(0x1A1A1A) };
    let subtle_hover = if dark {
        Srgba::hex(0xFFFFFF).with_alpha(0.06)
    } else {
        Srgba::hex(0x000000).with_alpha(0.05)
    };
    let subtle_pressed = if dark {
        Srgba::hex(0xFFFFFF).with_alpha(0.04)
    } else {
        Srgba::hex(0x000000).with_alpha(0.03)
    };
    let plain = ButtonColors {
        fill_idle: Srgba::TRANSPARENT,
        fill_hover: subtle_hover,
        fill_pressed: subtle_pressed,
        fill_unfocused: Srgba::TRANSPARENT,
        glyph: text,
        glyph_hover: text,
    };

    DecoTheme {
        style: ChromeStyle::Win11,
        scheme: Scheme::Mono,
        mode,
        metrics: DecoMetrics {
            titlebar_height: 32.0,
            border_thickness: 1.0,
            resize_band: 8.0,
            corner_radius: 8.0,
            title_size_px: pt_to_px(DEFAULT_TITLE_SIZE_PT),
            title_font_family: default_title_font_family(),
            title_font_weight: DEFAULT_TITLE_FONT_WEIGHT,
            title_align: TitleAlign::Leading,
            title_pad: 12.0,
            shadow: ShadowSpec {
                softness: 22.0,
                offset_y: 6.0,
                color: Srgba::hex(0x000000),
                alpha_focused: 0.30,
                alpha_unfocused: 0.15,
            },
        },
        colors: DecoColors {
            titlebar_focused: if dark { Srgba::hex(0x202020) } else { Srgba::hex(0xF3F3F3) },
            titlebar_unfocused: if dark { Srgba::hex(0x272727) } else { Srgba::hex(0xEBEBEB) },
            titlebar_divider: Srgba::TRANSPARENT,
            title_text_focused: text,
            title_text_unfocused: if dark { Srgba::hex(0x9B9B9B) } else { Srgba::hex(0x8A8A8A) },
            border_focused: if dark { Srgba::hex(0x3A3A3A) } else { Srgba::hex(0xD8D8D8) },
            border_unfocused: if dark { Srgba::hex(0x2E2E2E) } else { Srgba::hex(0xE2E2E2) },
        },
        buttons: ButtonCluster {
            side: ButtonSide::Right,
            // Outermost first: close hugs the corner, then maximize, minimize.
            order: [CaptionButton::Close, CaptionButton::Maximize, CaptionButton::Minimize],
            shape: ButtonShape::FullHeightRect { width: 38.0 },
            gap: 0.0,
            edge_inset: 0.0,
            // 0.40, not the 0.36 the other two use: the cell is transparent at
            // rest, so the glyph is the only thing on screen and has to read as
            // a target on its own (Mark, 2026-08-11, on the 38px cell).
            glyph_extent_ratio: 0.40,
            glyphs: GlyphPolicy::Always,
            close: ButtonColors {
                fill_idle: Srgba::TRANSPARENT,
                fill_hover: Srgba::hex(0xC42B1C),
                fill_pressed: Srgba::hex(0xC42B1C).with_alpha(0.9),
                fill_unfocused: Srgba::TRANSPARENT,
                glyph: text,
                glyph_hover: Srgba::hex(0xFFFFFF),
            },
            minimize: plain,
            maximize: plain,
        },
    }
}

// ---------------------------------------------------------------- cosmix

/// The native CosMix chrome. Neutral surfaces follow the mode; the focus
/// ring, hover fills and pressed states all carry the ctk scheme accent, so
/// switching schemes desktop-wide re-tints every titlebar in lockstep with
/// ctk apps. Circular buttons on the right, glyphs always visible, and an
/// accent border that doubles as the focus indicator.
pub fn cosmix(scheme: Scheme, mode: Mode) -> DecoTheme {
    let dark = mode == Mode::Dark;

    // Per-scheme accents: same hue table as ctk::theme builtin palettes
    // (scheme = hue; L/C tuned per mode, reds punchier, stone/mono muted).
    let (hue, chroma) = match scheme {
        Scheme::Ocean => (220.0, 0.12),
        Scheme::Crimson => (25.0, 0.21),
        Scheme::Stone => (60.0, 0.04),
        Scheme::Forest => (150.0, 0.12),
        Scheme::Sunset => (45.0, 0.15), // ctk theme.rs Sunset hue — keep in lockstep
        Scheme::Mono => (0.0, 0.0),
    };
    let accent = if dark { oklch(75.0, chroma, hue) } else { oklch(52.0, chroma, hue) };
    let accent_soft = accent.with_alpha(0.20);
    let accent_pressed = accent.with_alpha(0.35);

    let glyph = if dark { Srgba::hex(0xE6E6E6) } else { Srgba::hex(0x2A2A2A) };
    let button = |hover_is_danger: bool| ButtonColors {
        fill_idle: if dark {
            Srgba::hex(0xFFFFFF).with_alpha(0.07)
        } else {
            Srgba::hex(0x000000).with_alpha(0.06)
        },
        fill_hover: if hover_is_danger { oklch(55.0, 0.19, 25.0) } else { accent_soft },
        fill_pressed: if hover_is_danger {
            oklch(48.0, 0.19, 25.0)
        } else {
            accent_pressed
        },
        fill_unfocused: if dark {
            Srgba::hex(0xFFFFFF).with_alpha(0.04)
        } else {
            Srgba::hex(0x000000).with_alpha(0.03)
        },
        glyph,
        glyph_hover: if hover_is_danger { Srgba::hex(0xFFFFFF) } else { glyph },
    };

    DecoTheme {
        style: ChromeStyle::Cosmix,
        scheme,
        mode,
        metrics: DecoMetrics {
            titlebar_height: 30.0,
            border_thickness: 1.0,
            resize_band: 8.0,
            corner_radius: 10.0,
            title_size_px: pt_to_px(DEFAULT_TITLE_SIZE_PT),
            title_font_family: default_title_font_family(),
            title_font_weight: DEFAULT_TITLE_FONT_WEIGHT,
            title_align: TitleAlign::Leading,
            title_pad: 10.0,
            shadow: ShadowSpec {
                softness: 28.0,
                offset_y: 8.0,
                color: Srgba::hex(0x000000),
                alpha_focused: 0.35,
                alpha_unfocused: 0.18,
            },
        },
        colors: DecoColors {
            titlebar_focused: if dark { Srgba::hex(0x232326) } else { Srgba::hex(0xF2F1F3) },
            titlebar_unfocused: if dark { Srgba::hex(0x1D1D1F) } else { Srgba::hex(0xF7F6F8) },
            titlebar_divider: if dark {
                Srgba::hex(0x000000).with_alpha(0.3)
            } else {
                Srgba::hex(0x000000).with_alpha(0.08)
            },
            title_text_focused: if dark { Srgba::hex(0xEDEDED) } else { Srgba::hex(0x232326) },
            title_text_unfocused: if dark { Srgba::hex(0x8A8A8E) } else { Srgba::hex(0x9C9CA0) },
            // The accent border is the CosMix focus signature.
            border_focused: accent,
            border_unfocused: if dark { Srgba::hex(0x333336) } else { Srgba::hex(0xDCDCDE) },
        },
        buttons: ButtonCluster {
            side: ButtonSide::Right,
            order: [CaptionButton::Close, CaptionButton::Maximize, CaptionButton::Minimize],
            // 18px, up from 16 — Mark read 16 as "okay, maybe a little too
            // small" on the Phase 3 close smoke (2026-08-11). The 30px
            // titlebar still leaves 6px of breathing room above and below.
            shape: ButtonShape::Circle { diameter: 18.0 },
            gap: 6.0,
            edge_inset: 10.0,
            // The 18px disc carries a visible idle fill, so the glyph sits
            // inside an affordance rather than being one.
            glyph_extent_ratio: 0.36,
            glyphs: GlyphPolicy::Always,
            close: button(true),
            minimize: button(false),
            maximize: button(false),
        },
    }
}

/// Resolve a full theme from the selection triple — the single entry point
/// cosmix-comp calls after reading configuration.
pub fn resolve(style: ChromeStyle, scheme: Scheme, mode: Mode) -> DecoTheme {
    let mut theme = match style {
        ChromeStyle::Mac => mac(mode),
        ChromeStyle::Win11 => win11(mode),
        ChromeStyle::Cosmix => cosmix(scheme, mode),
    };
    // Mac/win11 palettes ignore the scheme, but the resolved triple must
    // still record the caller's selection — a later style switch re-resolves
    // from `theme.scheme`, and Mono here would silently drop the choice.
    theme.scheme = scheme;
    theme
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{ButtonState, Focus};

    #[test]
    fn every_style_requests_the_same_desktop_title_face() {
        for style in ChromeStyle::ALL {
            let m = resolve(style, Scheme::Ocean, Mode::Light).metrics;
            assert_eq!(
                m.title_font_family,
                DecoFontFamily::Named(DEFAULT_TITLE_FONT_FAMILY.to_owned()),
                "{style:?} must letter its titles like the rest of the desktop"
            );
            assert_eq!(m.title_font_weight, DecoFontWeight::LIGHT, "{style:?}");
            assert_eq!(m.title_size_px, pt_to_px(11.0), "{style:?}");
            assert!(
                m.title_size_px * 1.5 < m.titlebar_height,
                "{style:?} titlebar must have room for the default face"
            );
        }
    }

    #[test]
    fn resolve_records_the_selected_triple() {
        for style in ChromeStyle::ALL {
            let t = resolve(style, Scheme::Ocean, Mode::Dark);
            assert_eq!(t.style, style);
            assert_eq!(t.scheme, Scheme::Ocean, "{style:?} must record the caller's scheme");
            assert_eq!(t.mode, Mode::Dark);
        }
    }

    #[test]
    fn resolve_covers_every_triple() {
        for style in ChromeStyle::ALL {
            for scheme in Scheme::ALL {
                for mode in [Mode::Dark, Mode::Light] {
                    let t = resolve(style, scheme, mode);
                    assert_eq!(t.style, style);
                    assert_eq!(t.mode, mode);
                    assert!(t.metrics.titlebar_height > 0.0);
                }
            }
        }
    }

    #[test]
    fn win11_close_hover_is_the_red_one() {
        let t = win11(Mode::Dark);
        let hover = t.buttons.close.fill(ButtonState::Hover, Focus::Focused);
        assert!(hover.r > 0.7 && hover.g < 0.3, "expected the signature red");
        // And idle is invisible, like the real thing.
        assert_eq!(t.buttons.close.fill(ButtonState::Idle, Focus::Focused).a, 0.0);
    }

    /// Pins the two button sizes Mark tuned by eye on the Phase 3 close smoke
    /// (2026-08-11). Both are *deliberate divergences from their reference* —
    /// win11's caption cell is 38px where real Windows 11 is 46px — so without
    /// this test the next person to check the numbers against the reference
    /// would "correct" them straight back to what was rejected.
    #[test]
    fn button_sizes_are_the_hand_tuned_ones_not_the_reference_ones() {
        assert_eq!(
            win11(Mode::Light).buttons.shape,
            ButtonShape::FullHeightRect { width: 38.0 },
            "38px is Mark's call, not Windows 11's 46px — see the win11() doc comment"
        );
        assert_eq!(
            cosmix(Scheme::Ocean, Mode::Light).buttons.shape,
            ButtonShape::Circle { diameter: 18.0 },
            "16px read as slightly too small on the close smoke"
        );
    }

    /// The glyph ratio is per-style, and the reason is the idle fill: a style
    /// whose button is invisible until hovered has nothing on screen but the
    /// glyph, so the glyph has to be bigger. If a style ever gains or loses an
    /// idle fill, its ratio should be revisited — this test is where that
    /// coupling is written down.
    #[test]
    fn styles_without_an_idle_button_fill_draw_a_larger_glyph() {
        for (name, theme) in [
            ("mac", mac(Mode::Light)),
            ("win11", win11(Mode::Light)),
            ("cosmix", cosmix(Scheme::Ocean, Mode::Light)),
        ] {
            let idle_is_invisible =
                theme.buttons.close.fill(ButtonState::Idle, Focus::Focused).a == 0.0;
            let ratio = theme.buttons.glyph_extent_ratio;
            if idle_is_invisible {
                assert!(
                    ratio >= 0.40,
                    "{name} draws no idle fill, so its glyph is the whole target — \
                     {ratio} is too timid"
                );
            } else {
                assert!(
                    ratio < 0.40,
                    "{name} fills its button at rest, so the glyph is a detail \
                     inside an affordance, not the affordance — {ratio} is too loud"
                );
            }
        }
    }

    /// The win11 glyph is sized off `min(cell_w, cell_h)`, so the narrowing
    /// above must not have shrunk the glyph as a side effect — the cell has to
    /// stay wider than the titlebar is tall for the height to remain the
    /// binding dimension.
    #[test]
    fn narrowing_the_win11_cell_left_the_glyph_metric_alone() {
        let t = win11(Mode::Light);
        let ButtonShape::FullHeightRect { width } = t.buttons.shape else {
            panic!("win11 buttons are full-height rects");
        };
        assert!(
            width > t.metrics.titlebar_height,
            "cell {width} must stay wider than the {}px titlebar, or the glyph \
             starts scaling with cell width",
            t.metrics.titlebar_height
        );
    }

    #[test]
    fn mac_traffic_lights_mute_when_unfocused() {
        let t = mac(Mode::Dark);
        let focused = t.buttons.close.fill(ButtonState::Idle, Focus::Focused);
        let unfocused = t.buttons.close.fill(ButtonState::Idle, Focus::Unfocused);
        assert_ne!(focused, unfocused);
        // But hover re-lights even without focus.
        assert_eq!(t.buttons.close.fill(ButtonState::Hover, Focus::Unfocused), focused);
    }

    #[test]
    fn cosmix_focus_border_tracks_scheme() {
        let ocean = cosmix(Scheme::Ocean, Mode::Dark).colors.border_focused;
        let crimson = cosmix(Scheme::Crimson, Mode::Dark).colors.border_focused;
        assert_ne!(ocean, crimson, "schemes must produce distinct accents");
        let mono = cosmix(Scheme::Mono, Mode::Dark).colors.border_focused;
        assert!((mono.r - mono.g).abs() < 1e-3 && (mono.g - mono.b).abs() < 1e-3, "mono stays grey");
    }
}
