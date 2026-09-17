//! Colours, from the same `cosmix-design` compile the Bevy arm's CTK theme
//! resolves: the embedded revision-1 default source in the Ocean/Dark context,
//! mapped into iced through `cosmix_iced_widgets::Tokens`.
//!
//! A fixed context, not the user's theme files, so every run and both arms
//! draw the same colours. A design that will not compile is fatal: the arm
//! would otherwise report a measurement of a surface nobody specified.

use cosmix_design::{DesignCompileResult, DesignContext, Mode, Scheme, SourceIdentity};
use cosmix_iced_widgets::Tokens;
use iced::theme::Palette;
use iced::{Color, Theme};

/// The scheme both arms fix themselves to (the Bevy arm's `Scheme::Ocean`,
/// `ThemeMode::Dark`).
const SCHEME: Scheme = Scheme::Ocean;
const MODE: Mode = Mode::Dark;

/// Compiles the embedded design in the bake-off's fixed context.
pub fn tokens() -> Result<Tokens, String> {
    let document = cosmix_design::parse_design_source(
        SourceIdentity::new("embedded:bench-mixer-iced"),
        cosmix_design::EMBEDDED_DEFAULT_SOURCE,
    )
    .map_err(|error| format!("the embedded design source does not parse: {error:?}"))?;
    let context = DesignContext {
        scheme: SCHEME,
        mode: MODE,
        ..Default::default()
    };
    let DesignCompileResult::Success(compiled) = cosmix_design::compile_design(&document, context)
    else {
        return Err(format!(
            "the embedded design does not compile for ({}, {})",
            SCHEME.name(),
            MODE.name()
        ));
    };
    Tokens::from_dictionary(compiled.candidate.dictionary())
        .map_err(|error| format!("the compiled design is missing a widget token: {error}"))
}

/// The iced theme for text and containers. The pro-audio controls take
/// `tokens.audio_style()` explicitly and do not read this.
pub fn iced_theme(tokens: Tokens) -> Theme {
    Theme::custom(
        "cosmix-bench",
        Palette {
            background: tokens.surface,
            text: tokens.text,
            primary: tokens.primary,
            success: tokens.primary,
            warning: tokens.selection,
            danger: tokens.destructive,
        },
    )
}

/// Background of a channel strip.
///
/// The Bevy arm has three background rungs to work with — `ctk.surface`,
/// `ctk.panel` and `ctk.master.panel` are the design's `palette.background.1`,
/// `.2` and `.3`, which CTK's `web_anchor_verbatim` test pins to the compiled
/// design. `cosmix_iced_widgets::Tokens` maps colour *pairs* instead, and in
/// the Ocean/Dark design `base`, `card`, `popover` and `muted` all resolve to
/// the same near-black (#020709), so taking the panels from there would draw a
/// single flat board with no strip edges at all.
///
/// So the two lifts come from the next two colours this design does separate:
/// `border` for the channel panels and `selection` for the master's further
/// lift. The exact values are not CTK's bg2/bg3 — that is an accepted parity
/// delta (`known-deltas.conf.mix`, `colour-role-mapping`) and it goes away
/// when `Tokens` exposes the design's background rungs.
/// [`the_panels_are_three_distinct_rungs`] fails rather than letting the board
/// go flat if the design ever moves.
pub fn strip_background(tokens: Tokens, master: bool) -> Color {
    if master { tokens.selection } else { tokens.border }
}

/// Perceptual-ish ordering key; only used to assert the rungs stay separated.
#[cfg(test)]
fn luma(colour: Color) -> f32 {
    0.2126 * colour.r + 0.7152 * colour.g + 0.0722 * colour.b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_design_resolves_every_widget_token() {
        let tokens = tokens().expect("the embedded Ocean/Dark design compiles");
        // A dark scheme: the surface must be darker than the text on it,
        // which also proves the linear -> sRGB conversion did not invert.
        assert!(luma(tokens.surface) < luma(tokens.text));
        assert!(tokens.radius > 0.0);
        let audio = tokens.audio_style();
        assert_eq!(audio.background, tokens.card);
        assert_ne!(audio.meter_clip, audio.meter_low);
    }

    /// The board must read as window, strip, master: three separated rungs,
    /// each lighter than the last, with the fader and meter wells sunk back
    /// into the darkest of them. If a design change collapses any of that,
    /// this fails instead of the arm quietly drawing a flat rectangle.
    #[test]
    fn the_panels_are_three_distinct_rungs() {
        let tokens = tokens().unwrap();
        let window = tokens.surface;
        let channel = strip_background(tokens, false);
        let master = strip_background(tokens, true);
        assert!(
            luma(window) < luma(channel) && luma(channel) < luma(master),
            "window {window:?} < channel {channel:?} < master {master:?} must hold"
        );
        // The fader and meter wells have to stay visible inside a strip.
        let well = tokens.audio_style().track;
        assert!(luma(well) < luma(channel), "well {well:?} in {channel:?}");
    }

    /// Not an assertion: the resolved palette, so a reader can see which
    /// rungs this design actually separates (`cargo test -- --nocapture`).
    #[test]
    fn the_resolved_palette_is_reported() {
        let t = tokens().unwrap();
        for (name, colour) in [
            ("surface", t.surface),
            ("card", t.card),
            ("popover", t.popover),
            ("muted_surface", t.muted_surface),
            ("input", t.input),
            ("border", t.border),
            ("selection", t.selection),
            ("primary", t.primary),
            ("ring", t.ring),
            ("text", t.text),
        ] {
            let [r, g, b] = [colour.r, colour.g, colour.b].map(|c| (c * 255.0).round() as u8);
            println!("palette {name:<14} #{r:02x}{g:02x}{b:02x}");
        }
    }

    #[test]
    fn the_theme_carries_the_resolved_palette() {
        let tokens = tokens().unwrap();
        let theme = iced_theme(tokens);
        assert_eq!(theme.palette().background, tokens.surface);
        assert_eq!(theme.palette().text, tokens.text);
    }
}
